//! Consensus component for the node builder.
use reth_node_api::{EngineValidator, NodeTypesWithEngine};

use crate::{BuilderContext, FullNodeTypes};
use reth_bsc_consensus::Parlia;
use std::future::Future;

/// A type that knows how to build the engine validator.
pub trait EngineValidatorBuilder<Node: FullNodeTypes>: Send {
    /// The consensus implementation to build.
    type Validator: EngineValidator<<Node::Types as NodeTypesWithEngine>::Engine>
        + Clone
        + Unpin
        + 'static;

    /// Creates the engine validator.
    fn build_validator(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<Self::Validator>> + Send;
}

impl<Node, F, Fut, Validator> EngineValidatorBuilder<Node> for F
where
    Node: FullNodeTypes,
    Validator:
        EngineValidator<<Node::Types as NodeTypesWithEngine>::Engine> + Clone + Unpin + 'static,
    F: FnOnce(&BuilderContext<Node>) -> Fut + Send,
    Fut: Future<Output = eyre::Result<Validator>> + Send,
{
    type Validator = Validator;

    fn build_validator(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<Self::Validator>> {
        self(ctx)
    }
}

/// Needed for bsc parlia consensus.
pub trait ParliaBuilder<Node: FullNodeTypes>: Send {
    /// Creates the parlia.
    fn build_parlia(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<Parlia>> + Send;
}

impl<Node, F, Fut> ParliaBuilder<Node> for F
where
    Node: FullNodeTypes,
    F: FnOnce(&BuilderContext<Node>) -> Fut + Send,
    Fut: Future<Output = eyre::Result<Parlia>> + Send,
{
    fn build_parlia(
        self,
        ctx: &BuilderContext<Node>,
    ) -> impl Future<Output = eyre::Result<Parlia>> {
        self(ctx)
    }
}
