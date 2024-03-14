use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use arrow::datatypes::IntervalMonthDayNanoType;

use arroyo_datastream::logical::{LogicalEdge, LogicalGraph, LogicalNode};
use arroyo_rpc::df::{ArroyoSchema, ArroyoSchemaRef};

use async_trait::async_trait;
use datafusion::execution::context::SessionState;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::physical_planner::{DefaultPhysicalPlanner, ExtensionPlanner, PhysicalPlanner};
use datafusion_common::tree_node::{TreeNode, TreeNodeVisitor, VisitRecursion};
use datafusion_common::{
    DFSchema, DFSchemaRef, DataFusionError, OwnedTableReference, Result as DFResult, ScalarValue,
};
use datafusion_execution::config::SessionConfig;
use datafusion_execution::runtime_env::{RuntimeConfig, RuntimeEnv};
use datafusion_expr::expr::ScalarFunction;
use datafusion_expr::{
    BuiltinScalarFunction, Expr, Extension, LogicalPlan, UserDefinedLogicalNode,
};
use datafusion_physical_expr::PhysicalExpr;
use datafusion_proto::protobuf::{PhysicalExprNode, PhysicalPlanNode};
use petgraph::graph::{DiGraph, NodeIndex};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

use crate::extension::key_calculation::KeyCalculationExtension;
use crate::extension::{ArroyoExtension, NodeWithIncomingEdges};
use crate::physical::{new_registry, ArroyoMemExec, ArroyoPhysicalExtensionCodec, DecodingContext};
use crate::schemas::add_timestamp_field_arrow;
use datafusion_proto::{
    physical_plan::AsExecutionPlan,
    protobuf::{physical_plan_node::PhysicalPlanType, AggregateMode},
};

#[derive(Default)]
pub(crate) struct PlanToGraphVisitor {
    graph: DiGraph<LogicalNode, LogicalEdge>,
    output_schemas: HashMap<NodeIndex, ArroyoSchemaRef>,
    named_nodes: HashMap<NamedNode, NodeIndex>,
    // each node that needs to know its inputs should push an empty vec in pre_visit.
    // In post_visit each node should cleanup its vec and push its index to the last vec, if present.
    traversal: Vec<Vec<NodeIndex>>,
    planner: Planner,
}

pub(crate) struct Planner {
    planner: DefaultPhysicalPlanner,
    session_state: SessionState,
}

impl Default for Planner {
    fn default() -> Self {
        let planner = DefaultPhysicalPlanner::with_extension_planners(vec![Arc::new(
            ArroyoExtensionPlanner {},
        )]);
        let mut config = SessionConfig::new();
        config
            .options_mut()
            .optimizer
            .enable_round_robin_repartition = false;
        config.options_mut().optimizer.repartition_aggregations = false;
        config.options_mut().optimizer.repartition_windows = false;
        config.options_mut().optimizer.repartition_sorts = false;
        let session_state =
            SessionState::new_with_config_rt(config, Arc::new(RuntimeEnv::default()))
                .with_physical_optimizer_rules(vec![]);
        Self {
            planner,
            session_state,
        }
    }
}

impl Planner {
    pub(crate) fn sync_plan(&self, plan: &LogicalPlan) -> DFResult<Arc<dyn ExecutionPlan>> {
        let fut = self.planner.create_physical_plan(plan, &self.session_state);
        let (tx, mut rx) = oneshot::channel();
        thread::scope(|s| {
            let _handle = tokio::runtime::Handle::current();
            s.spawn(move || {
                let rt = Runtime::new().unwrap();
                rt.block_on(async {
                    let plan = fut.await;
                    tx.send(plan).unwrap();
                });
            });
        });

        rx.try_recv().unwrap()
    }
    pub(crate) fn create_physical_expr(
        &self,
        expr: &Expr,
        input_dfschema: &DFSchema,
    ) -> DFResult<Arc<dyn PhysicalExpr>> {
        self.planner
            .create_physical_expr(expr, input_dfschema, &self.session_state)
    }

    // This splits aggregates into two parts, the partial aggregation and the final aggregation.
    // This needs to be done in physical space as that's the only point at which this split is realized.
    pub(crate) fn split_physical_plan(
        &self,
        key_indices: Vec<usize>,
        aggregate: &LogicalPlan,
    ) -> DFResult<SplitPlanOutput> {
        let physical_plan = self.sync_plan(aggregate)?;
        let codec = ArroyoPhysicalExtensionCodec {
            context: DecodingContext::Planning,
        };
        let mut physical_plan_node =
            PhysicalPlanNode::try_from_physical_plan(physical_plan.clone(), &codec)?;
        let PhysicalPlanType::Aggregate(mut final_aggregate_proto) = physical_plan_node
            .physical_plan_type
            .take()
            .ok_or_else(|| DataFusionError::Plan("missing physical plan type".to_string()))?
        else {
            return Err(DataFusionError::Plan(
                "unexpected physical plan type".to_string(),
            ));
        };
        let AggregateMode::Final = final_aggregate_proto.mode() else {
            return Err(DataFusionError::Plan(
                "unexpected physical plan type".to_string(),
            ));
        };
        // pull out the partial aggregation, so we can checkpoint it.
        let partial_aggregation_plan = *final_aggregate_proto
            .input
            .take()
            .ok_or_else(|| DataFusionError::Plan("missing input".to_string()))?;

        // need to convert to ExecutionPlan to get the partial schema.
        let partial_aggregation_exec_plan = partial_aggregation_plan.try_into_physical_plan(
            &new_registry(),
            &RuntimeEnv::new(RuntimeConfig::new()).unwrap(),
            &codec,
        )?;

        let partial_schema = partial_aggregation_exec_plan.schema();
        let final_input_table_provider = ArroyoMemExec {
            table_name: "partial".into(),
            schema: partial_schema.clone(),
        };

        final_aggregate_proto.input = Some(Box::new(PhysicalPlanNode::try_from_physical_plan(
            Arc::new(final_input_table_provider),
            &codec,
        )?));

        let finish_plan = PhysicalPlanNode {
            physical_plan_type: Some(PhysicalPlanType::Aggregate(final_aggregate_proto)),
        };

        let partial_schema = ArroyoSchema::new_keyed(
            add_timestamp_field_arrow(partial_schema.clone()),
            partial_schema.fields().len(),
            key_indices,
        );

        Ok(SplitPlanOutput {
            partial_aggregation_plan,
            partial_schema,
            finish_plan,
        })
    }

    pub fn binning_function_proto(
        &self,
        width: Duration,
        input_schema: DFSchemaRef,
    ) -> DFResult<PhysicalExprNode> {
        let date_bin = Expr::ScalarFunction(ScalarFunction {
            func_def: datafusion_expr::ScalarFunctionDefinition::BuiltIn(
                BuiltinScalarFunction::DateBin,
            ),
            args: vec![
                Expr::Literal(ScalarValue::IntervalMonthDayNano(Some(
                    IntervalMonthDayNanoType::make_value(0, 0, width.as_nanos() as i64),
                ))),
                Expr::Column(datafusion_common::Column {
                    relation: None,
                    name: "_timestamp".into(),
                }),
            ],
        });

        let binning_function = self.create_physical_expr(&date_bin, &input_schema)?;
        PhysicalExprNode::try_from(binning_function)
    }
}

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub(crate) enum NamedNode {
    Source(OwnedTableReference),
    Watermark(OwnedTableReference),
    RemoteTable(OwnedTableReference),
}

struct ArroyoExtensionPlanner {}

#[async_trait]
impl ExtensionPlanner for ArroyoExtensionPlanner {
    async fn plan_extension(
        &self,
        _planner: &dyn PhysicalPlanner,
        node: &dyn UserDefinedLogicalNode,
        _logical_inputs: &[&LogicalPlan],
        _physical_inputs: &[Arc<dyn ExecutionPlan>],
        _session_state: &SessionState,
    ) -> DFResult<Option<Arc<dyn ExecutionPlan>>> {
        let schema = node.schema().as_ref().into();
        let name =
            if let Some(key_extension) = node.as_any().downcast_ref::<KeyCalculationExtension>() {
                key_extension.name.clone()
            } else {
                None
            };
        Ok(Some(Arc::new(ArroyoMemExec {
            table_name: name.unwrap_or("memory".to_string()),
            schema: Arc::new(schema),
        })))
    }
}

impl PlanToGraphVisitor {
    fn add_index_to_traversal(&mut self, index: NodeIndex) {
        if let Some(last) = self.traversal.last_mut() {
            last.push(index);
        }
    }

    pub(crate) fn add_plan(&mut self, plan: LogicalPlan) -> DFResult<()> {
        self.traversal.clear();
        plan.visit(self)?;
        Ok(())
    }

    pub fn into_graph(self) -> LogicalGraph {
        self.graph
    }

    pub fn build_extension(
        &mut self,
        input_nodes: Vec<NodeIndex>,
        extension: &dyn ArroyoExtension,
    ) -> DFResult<()> {
        if let Some(node_name) = extension.node_name() {
            if self.named_nodes.contains_key(&node_name) {
                // we should've short circuited
                return Err(DataFusionError::Plan(format!(
                    "extension {:?} has already been planned, shouldn't try again.",
                    node_name
                )));
            }
        }
        let input_schemas = input_nodes
            .iter()
            .map(|index| {
                Ok(self
                    .output_schemas
                    .get(index)
                    .ok_or_else(|| DataFusionError::Plan("missing input node".to_string()))?
                    .clone())
            })
            .collect::<DFResult<Vec<_>>>()?;
        let NodeWithIncomingEdges { node, edges } = extension
            .plan_node(&self.planner, self.graph.node_count(), input_schemas)
            .map_err(|e| DataFusionError::Plan(format!("error planning extension: {}", e)))?;
        let node_index = self.graph.add_node(node);
        self.add_index_to_traversal(node_index);
        for (source, edge) in input_nodes.into_iter().zip(edges.into_iter()) {
            self.graph.add_edge(source, node_index, edge);
        }
        self.output_schemas
            .insert(node_index, extension.output_schema().into());
        if let Some(node_name) = extension.node_name() {
            self.named_nodes.insert(node_name, node_index);
        }
        Ok(())
    }
}

impl TreeNodeVisitor for PlanToGraphVisitor {
    type N = LogicalPlan;

    fn pre_visit(&mut self, node: &Self::N) -> DFResult<VisitRecursion> {
        let LogicalPlan::Extension(Extension { node }) = node else {
            return Ok(VisitRecursion::Continue);
        };
        let arroyo_extension: &dyn ArroyoExtension = node
            .try_into()
            .map_err(|e| DataFusionError::Plan(format!("error converting extension: {}", e)))?;
        if let Some(name) = arroyo_extension.node_name() {
            if let Some(node_index) = self.named_nodes.get(&name) {
                self.add_index_to_traversal(*node_index);
                return Ok(VisitRecursion::Skip);
            }
        }

        if !node.inputs().is_empty() {
            self.traversal.push(vec![]);
        }

        Ok(VisitRecursion::Continue)
    }

    // most of the work sits in post visit so that we can have the inputs of each node
    fn post_visit(&mut self, node: &Self::N) -> DFResult<VisitRecursion> {
        let LogicalPlan::Extension(Extension { node }) = node else {
            return Ok(VisitRecursion::Continue);
        };

        let input_nodes = if !node.inputs().is_empty() {
            self.traversal.pop().unwrap_or_default()
        } else {
            vec![]
        };
        let arroyo_extension: &dyn ArroyoExtension = node
            .try_into()
            .map_err(|e| DataFusionError::Plan(format!("error converting extension: {}", e)))?;
        self.build_extension(input_nodes, arroyo_extension)
            .map_err(|e| DataFusionError::Plan(format!("error building extension: {}", e)))?;

        Ok(VisitRecursion::Continue)
    }
}

pub(crate) struct SplitPlanOutput {
    pub(crate) partial_aggregation_plan: PhysicalPlanNode,
    pub(crate) partial_schema: ArroyoSchema,
    pub(crate) finish_plan: PhysicalPlanNode,
}