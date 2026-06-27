//! Closures and upvalue cells (port `lfunc.cpp`).
//!
//! A [`Closure`] pairs a prototype with the upvalue cells it captures. An
//! [`UpVal`] is open while it still points into a live thread's register stack,
//! or closed once it owns its value. The open form is `{thread, slot,
//! generation}`, replacing upstream's pointer patching: the
//! generation guards against the thread's arena slot being reused.

use ruau_vm_api::{RawGc, RawValue, marker};

use crate::object::Proto;

/// A closure: a prototype plus its captured upvalue cells.
#[derive(serde::Deserialize, serde::Serialize)]
pub struct Closure {
    /// The prototype this closure runs.
    pub proto: RawGc<Proto>,
    /// Optional function environment used by the `fenv` compatibility feature.
    /// `None` means the closure uses the running thread's global table.
    pub env: Option<RawGc<marker::Table>>,
    /// Captured upvalue cells, in upvalue order.
    pub upvals: Vec<RawGc<UpVal>>,
}

impl Closure {
    /// A closure over `proto` with no upvalues bound yet (the binding happens at
    /// `NEWCLOSURE` during execution).
    #[must_use]
    pub fn new(proto: RawGc<Proto>) -> Self {
        Self {
            proto,
            env: None,
            upvals: Vec::new(),
        }
    }

    /// GC: visits the prototype and captured upvalue cells.
    pub(crate) fn gc_trace<V: crate::gc::GcVisit>(
        &self,
        v: &mut V,
    ) -> Result<(), crate::gc::GcAbort> {
        use crate::gc::GcRef;
        v.visit(GcRef::Proto(self.proto.index()), self.proto.generation())?;
        if let Some(env) = self.env {
            v.visit(GcRef::Table(env.index()), env.generation())?;
        }
        for upval in &self.upvals {
            v.visit(GcRef::UpVal(upval.index()), upval.generation())?;
        }
        Ok(())
    }
}

/// An upvalue cell.
#[derive(serde::Deserialize, serde::Serialize)]
pub enum UpVal {
    /// Open: references register `slot` of `thread`. The handle's own
    /// generation guards against the thread's arena slot being reused.
    Open {
        /// The owning thread.
        thread: RawGc<marker::Thread>,
        /// Register slot index within that thread's stack.
        slot: u32,
    },
    /// Closed: owns its value (after the stack slot left scope).
    Closed(RawValue),
}

impl UpVal {
    /// GC: an open cell keeps its owning thread alive (the captured value lives in
    /// that thread's register slot, traced there); a closed cell owns its value.
    pub(crate) fn gc_trace<V: crate::gc::GcVisit>(
        &self,
        v: &mut V,
    ) -> Result<(), crate::gc::GcAbort> {
        use crate::gc::GcRef;
        match self {
            Self::Open { thread, .. } => {
                v.visit(GcRef::Thread(thread.index()), thread.generation())?;
            }
            Self::Closed(value) => {
                if let Some((child, generation)) = GcRef::from_value_gen(*value) {
                    v.visit(child, generation)?;
                }
            }
        }
        Ok(())
    }
}
