use super::{Graph, InputId, ScalarId, ScalarType};
use crate::{Function, Location, NativeOperation};
use std::sync::Arc;

pub type ValueId = usize;
pub type ConstantId = usize;
pub type ExitId = usize;

/// Runtime constants are held by the frontend; native code needs only IDs.
#[derive(Clone, Debug)]
pub enum Recipe {
    Constant(ConstantId),
    Scalar {
        value: ScalarId,
        ty: ScalarType,
    },
    Input(InputId),
    Struct(Vec<(String, ValueId)>),
    Union(String, ValueId),
    Vector(Vec<ValueId>),
    Deferred {
        operation: DeferredOperation,
        args: Vec<ValueId>,
        ty: crate::Type,
    },
}

#[derive(Clone, Debug)]
pub enum DeferredOperation {
    Intrinsic {
        name: String,
        width: Option<u32>,
    },
    Native {
        operation: NativeOperation,
        width: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Binding {
    pub name: String,
    pub value: ValueId,
}

#[derive(Clone, Debug)]
pub struct SourceFrame {
    pub function: Arc<Function>,
    pub locals: Arc<Vec<Binding>>,
    /// Dead values are omitted, but their binding scope must survive a resume.
    pub local_names: Arc<Vec<String>>,
    pub declared: Vec<u64>,
    /// Next source operation, not the beginning of the guest instruction.
    pub pc: usize,
    pub pending_result: Option<Location>,
    pub call_site: usize,
}

#[derive(Clone, Debug)]
pub struct Exit {
    /// Root-to-leaf order, matching the named interpreter's stack.
    pub frames: Vec<SourceFrame>,
    pub globals: Arc<Vec<Binding>>,
    pub completed_steps: u32,
    /// A child call/effect has completed but its caller has not assigned it.
    pub returned: Option<ValueId>,
    pub fetch_address: Option<ValueId>,
}

#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub globals: Arc<Vec<Binding>>,
    pub waiting: ValueId,
    pub completed_steps: u32,
    pub fetch_address: Option<ValueId>,
}

#[derive(Clone, Debug)]
pub struct CodeGuard {
    pub address: u64,
    pub len: usize,
    pub generation: u64,
    pub identity: u64,
    pub mapping: u64,
    pub first_page: u64,
    pub last_page: u64,
}

#[derive(Clone, Debug)]
pub struct ContextGuard {
    pub name: String,
    pub expected: ConstantId,
    pub version: u64,
}

#[derive(Clone, Debug)]
pub enum Event {
    Guard {
        condition: ScalarId,
        expected: bool,
        exit: ExitId,
    },
    ReadMemory {
        address: ScalarId,
        bytes: u8,
        exclusive: bool,
        result: ScalarId,
        error: ExitId,
    },
    WriteMemory {
        address: ScalarId,
        value: ScalarId,
        bytes: u8,
        exclusive: bool,
        result: Option<ScalarId>,
        error: ExitId,
        invalidated: ExitId,
    },
    Checkpoint(usize),
    Exit(ExitId),
}

#[derive(Clone, Debug, Default)]
pub struct Block {
    pub graph: Graph,
    pub values: Vec<Recipe>,
    pub events: Vec<Event>,
    pub exits: Vec<Exit>,
    pub checkpoints: Vec<Checkpoint>,
    pub code_guards: Vec<CodeGuard>,
    pub context_guards: Vec<ContextGuard>,
}

impl Block {
    pub fn value(&mut self, recipe: Recipe) -> ValueId {
        let id = self.values.len();
        self.values.push(recipe);
        id
    }

    pub fn exit(&mut self, snapshot: Exit) -> ExitId {
        let id = self.exits.len();
        self.exits.push(snapshot);
        id
    }

    pub fn checkpoint(&mut self, checkpoint: Checkpoint) {
        let id = self.checkpoints.len();
        self.checkpoints.push(checkpoint);
        self.events.push(Event::Checkpoint(id));
    }
}
