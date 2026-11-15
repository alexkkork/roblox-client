//! Local VM script-storage and owner-thread integrity observations.
//!
//! These counters describe operations that actually happen in this process.
//! They are not Roblox authentication or remote server integrity evidence.

use std::{
    collections::HashMap,
    rc::Rc,
    sync::{Arc, Weak},
    thread::{self, ThreadId},
};

use mlua::{
    chunk::{ChunkMode, Compiler},
    Function, Lua,
};
use serde_json::{json, Value as Json};

const MAX_SOURCE: usize = 100 * 1024 * 1024;
const MAX_BYTECODE: usize = 8 * 1024 * 1024;
const MAX_INTERNED_STRINGS: usize = 256;

#[derive(Default)]
struct Counters {
    thread_checks: u64,
    thread_failures: u64,
    constructions: u64,
    copies: u64,
    storage_failures: u64,
}

struct IntegrityState {
    owner: ThreadId,
    counters: Counters,
    strings: HashMap<Vec<u8>, Weak<[u8]>>,
}

impl IntegrityState {
    fn new() -> Self {
        Self {
            owner: thread::current().id(),
            counters: Counters::default(),
            strings: HashMap::new(),
        }
    }

    fn check_thread(&mut self, current: ThreadId) -> mlua::Result<()> {
        let matches = current == self.owner;
        self.counters.thread_checks += 1;
        if !matches {
            self.counters.thread_failures += 1;
            return Err(mlua::Error::RuntimeError(
                "script load attempted outside the DataModel owner thread".into(),
            ));
        }
        Ok(())
    }

    fn intern(&mut self, bytes: &[u8]) -> mlua::Result<Arc<[u8]>> {
        self.strings.retain(|_, value| value.strong_count() > 0);
        if let Some(value) = self.strings.get(bytes).and_then(Weak::upgrade) {
            return Ok(value);
        }
        if self.strings.len() >= MAX_INTERNED_STRINGS {
            return Err(mlua::Error::RuntimeError(
                "protected script storage limit exceeded".into(),
            ));
        }
        let stored: Arc<[u8]> = Arc::from(bytes);
        self.strings.insert(bytes.to_vec(), Arc::downgrade(&stored));
        Ok(stored)
    }
}

/// Immutable script bytes held by the session until the VM load completes.
/// Clone the payload explicitly at the load boundary to observe the same
/// source-handle identity check as the original host.
struct ProtectedSource {
    state: Rc<std::cell::RefCell<IntegrityState>>,
    source: Arc<[u8]>,
    bytecode: Arc<[u8]>,
}

impl ProtectedSource {
    fn new(
        state: Rc<std::cell::RefCell<IntegrityState>>,
        source: &[u8],
        bytecode: &[u8],
    ) -> mlua::Result<Self> {
        if source.len() > MAX_SOURCE || bytecode.len() > MAX_BYTECODE {
            return Err(mlua::Error::RuntimeError(
                "protected script storage input exceeds limit".into(),
            ));
        }

        let (stored_source, stored_bytecode) = {
            let mut state = state.borrow_mut();
            let source = state.intern(source)?;
            let bytecode = state.intern(bytecode)?;
            state.counters.constructions += 1;
            (source, bytecode)
        };

        if stored_source.as_ref() != source {
            state.borrow_mut().counters.storage_failures += 1;
            return Err(mlua::Error::RuntimeError(
                "protected script string construction mismatch".into(),
            ));
        }

        Ok(Self {
            state,
            source: stored_source,
            bytecode: stored_bytecode,
        })
    }

    fn checked_clone(&self) -> mlua::Result<Self> {
        let copy = Self {
            state: self.state.clone(),
            source: self.source.clone(),
            bytecode: self.bytecode.clone(),
        };
        let matches = Arc::ptr_eq(&copy.source, &self.source);
        {
            let mut state = copy.state.borrow_mut();
            state.counters.copies += 1;
            if !matches {
                state.counters.storage_failures += 1;
            }
        }
        if !matches {
            return Err(mlua::Error::RuntimeError(
                "protected script string copy mismatch".into(),
            ));
        }
        Ok(copy)
    }
}

/// Installs fresh, per-Lua-state observation state. The caller should invoke
/// this once before installing local source helpers or loading any script.
pub fn install(lua: &Lua) -> mlua::Result<()> {
    lua.set_app_data(Rc::new(std::cell::RefCell::new(IntegrityState::new())));
    Ok(())
}

fn observer(lua: &Lua) -> mlua::Result<Rc<std::cell::RefCell<IntegrityState>>> {
    lua.app_data_ref::<Rc<std::cell::RefCell<IntegrityState>>>()
        .map(|state| state.clone())
        .ok_or_else(|| mlua::Error::RuntimeError("VM integrity observer is not installed".into()))
}

/// Count the real OS-thread check made before script compilation/loading.
pub fn check_load_thread(lua: &Lua) -> mlua::Result<()> {
    let observer = observer(lua)?;
    let result = observer.borrow_mut().check_thread(thread::current().id());
    result
}

/// Compile source with mlua's official Luau compiler, keep the source and
/// actual generated bytecode in an interned immutable payload, validate the
/// payload copy, then ask the official VM to load those binary bytes.
pub fn load_source(lua: &Lua, name: &str, source: &str) -> mlua::Result<Function> {
    let state = observer(lua)?;
    state.borrow_mut().check_thread(thread::current().id())?;
    if source.len() > MAX_SOURCE {
        return Err(mlua::Error::RuntimeError(
            "loadstring source exceeds configured input limit".into(),
        ));
    }

    // The local debug APIs use Luau's public lua_getlocal/lua_setlocal
    // interface, which requires compiler-emitted LocVar metadata. Keep this
    // enabled for authored source so stack inspection works in actual loaded
    // scripts. Verified bytecode follows load_bytecode unchanged below.
    let bytecode = Compiler::new()
        .set_debug_level(2)
        .compile(source.as_bytes())?;
    let payload = ProtectedSource::new(state, source.as_bytes(), &bytecode)?;
    let load_payload = payload.checked_clone()?;
    lua.load(&*load_payload.bytecode)
        .set_name(name)
        .set_mode(ChunkMode::Binary)
        .into_function()
}

/// Load verified, host-provided bytecode through the same local owner-thread
/// and storage observations. `source` may be empty when no source is known.
pub fn load_bytecode(
    lua: &Lua,
    name: &str,
    source: &[u8],
    bytecode: &[u8],
) -> mlua::Result<Function> {
    let state = observer(lua)?;
    state.borrow_mut().check_thread(thread::current().id())?;
    let payload = ProtectedSource::new(state, source, bytecode)?;
    let load_payload = payload.checked_clone()?;
    lua.load(&*load_payload.bytecode)
        .set_name(name)
        .set_mode(ChunkMode::Binary)
        .into_function()
}

/// Snapshot actual local checks and storage operations. No synthetic values
/// are supplied when a path has not been exercised.
pub fn report(lua: &Lua) -> mlua::Result<(Json, Json)> {
    let state = observer(lua)?;
    let state = state.borrow();
    let thread = json!({
        "version": 1,
        "checks": state.counters.thread_checks,
        "failures": state.counters.thread_failures,
    });
    let protected = json!({
        "version": 1,
        "constructions": state.counters.constructions,
        "copies": state.counters.copies,
        "failures": state.counters.storage_failures,
    });
    Ok((thread, protected))
}

#[cfg(test)]
#[path = "integrity_tests.rs"]
mod tests;
