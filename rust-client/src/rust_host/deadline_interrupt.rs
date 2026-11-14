//! A scoped deadline hook through the official Luau callback API.
//!
//! No Lua values or user callbacks run here. Keeping the hot path this small
//! avoids mlua's general callback/error machinery on every loop backedge.

use mlua::{ffi, Lua, Result, Table};
use std::{cell::Cell, ptr, time::Instant};

type Interrupt = Option<unsafe extern "C-unwind" fn(*mut ffi::lua_State, i32)>;

pub(super) const MARKER_KEY: &str = "__rove_host_deadline_interrupt";
const MARKER_KEY_C: &[u8] = b"__rove_host_deadline_interrupt";

struct Budget {
    callbacks: *mut ffi::lua_Callbacks,
    deadline: Instant,
    hard_deadline: Instant,
    preempted: Cell<bool>,
    aborted: Cell<bool>,
}

thread_local! {
    // A scheduler resume is synchronous on its VM's owner thread. Nested
    // resumes save/restore this slot, and the Box keeps its address stable.
    static ACTIVE: Cell<*const Budget> = const { Cell::new(ptr::null()) };
}

unsafe extern "C-unwind" fn interrupt(state: *mut ffi::lua_State, gc: i32) {
    if gc >= 0 {
        return; // Luau GC callbacks cannot yield.
    }
    let budget = ACTIVE.with(Cell::get);
    if budget.is_null() {
        return;
    }
    // Match this exact VM, including its child coroutines; another VM on the
    // same OS thread must never consume the active VM's budget.
    let budget = unsafe { &*budget };
    if unsafe { ffi::lua_callbacks(state) } != budget.callbacks {
        return;
    }
    if Instant::now() >= budget.deadline {
        if unsafe { ffi::lua_isyieldable(state) } != 0 {
            budget.preempted.set(true);
            unsafe {
                // A normal Lua yield is deliberately distinguishable from
                // Luau's debug-break status through public coroutine APIs.
                // The host wrapper recognizes this private table marker and
                // propagates the preemption to the scheduler boundary.
                ffi::lua_pushlstring(state, MARKER_KEY_C.as_ptr().cast(), MARKER_KEY_C.len());
                ffi::lua_rawget(state, ffi::LUA_REGISTRYINDEX);
                ffi::lua_yield(state, 1);
            }
        } else if Instant::now() >= budget.hard_deadline {
            // A Lua callback inside a non-yieldable native operation (such as
            // table.sort) cannot be resumed later. End it through Luau's
            // protected error boundary instead of letting it hang the owner.
            budget.aborted.set(true);
            unsafe {
                ffi::lua_pushliteral(
                    state,
                    c"local Luau task exceeded its execution budget in a non-yieldable callback",
                );
                ffi::lua_error(state);
            }
        }
    }
}

pub(super) struct DeadlineInterrupt {
    budget: Box<Budget>,
    previous: Interrupt,
    previous_active: *const Budget,
}

pub(super) fn marker(lua: &Lua) -> Result<Table> {
    if let Ok(marker) = lua.named_registry_value::<Table>(MARKER_KEY) {
        return Ok(marker);
    }
    let marker = lua.create_table()?;
    lua.set_named_registry_value(MARKER_KEY, marker.clone())?;
    Ok(marker)
}

impl DeadlineInterrupt {
    pub(super) fn new(lua: &Lua, deadline: Instant, hard_deadline: Instant) -> Result<Self> {
        let _marker = marker(lua)?;
        let mut callbacks = ptr::null_mut();
        // exec_raw enters mlua's protected boundary only once per resume.
        unsafe {
            lua.exec_raw::<()>((), |state| callbacks = ffi::lua_callbacks(state))?;
        }
        let budget = Box::new(Budget {
            callbacks,
            deadline,
            hard_deadline: hard_deadline.max(deadline),
            preempted: Cell::new(false),
            aborted: Cell::new(false),
        });
        let previous = unsafe { (*callbacks).interrupt };
        let previous_active = ACTIVE.with(|active| active.replace(&*budget));
        unsafe {
            (*callbacks).interrupt = Some(interrupt);
        }
        Ok(Self {
            budget,
            previous,
            previous_active,
        })
    }

    pub(super) fn preempted(&self) -> bool {
        self.budget.preempted.get()
    }

    pub(super) fn aborted(&self) -> bool {
        self.budget.aborted.get()
    }
}

impl Drop for DeadlineInterrupt {
    fn drop(&mut self) {
        // The caller keeps the Lua owner alive for the whole resume scope.
        // Restore before dropping the Box so no callback retains its address.
        unsafe {
            (*self.budget.callbacks).interrupt = self.previous;
        }
        ACTIVE.with(|active| active.set(self.previous_active));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::{MultiValue, VmState};
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    #[test]
    fn deadline_yields_real_coroutine_and_restores_previous_hook() {
        let lua = Lua::new();
        let observed = Arc::new(AtomicUsize::new(0));
        let count = observed.clone();
        lua.set_interrupt(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            Ok(VmState::Continue)
        });
        let function = lua.load("while true do end").into_function().unwrap();
        let thread = lua.create_thread(function).unwrap();
        let deadline = Instant::now() + Duration::from_millis(2);
        let hook = DeadlineInterrupt::new(&lua, deadline, deadline).unwrap();
        thread.resume::<MultiValue>(()).unwrap();
        assert!(hook.preempted() && thread.is_resumable());
        drop(hook);
        let before = observed.load(Ordering::Relaxed);
        lua.load("local n=0; for i=1,100 do n+=i end; return n")
            .exec()
            .unwrap();
        assert!(observed.load(Ordering::Relaxed) > before);
    }

    #[test]
    fn nested_vm_deadlines_restore_the_outer_vm_hook_context() {
        let outer = Lua::new();
        let inner = Lua::new();
        let deadline = Instant::now();
        let outer_hook = DeadlineInterrupt::new(&outer, deadline, deadline).unwrap();
        {
            let deadline = Instant::now() + Duration::from_secs(1);
            let _inner_hook = DeadlineInterrupt::new(&inner, deadline, deadline).unwrap();
            assert_eq!(inner.load("return 42").eval::<i32>().unwrap(), 42);
        }
        let thread = outer
            .create_thread(outer.load("while true do end").into_function().unwrap())
            .unwrap();
        thread.resume::<MultiValue>(()).unwrap();
        assert!(outer_hook.preempted());
    }

    #[test]
    fn non_yieldable_lua_comparator_times_out_and_leaves_vm_usable() {
        let lua = Lua::new();
        let function = lua
            .load("table.sort({2,1}, function() while true do end end)")
            .into_function()
            .unwrap();
        let thread = lua.create_thread(function).unwrap();
        let deadline = Instant::now() + Duration::from_millis(2);
        let hook = DeadlineInterrupt::new(&lua, deadline, deadline).unwrap();
        let error = thread.resume::<MultiValue>(()).unwrap_err();
        assert!(hook.aborted());
        assert!(
            !hook.preempted(),
            "non-yieldable hard abort is not a soft preemption"
        );
        assert!(
            error.to_string().contains("non-yieldable callback"),
            "{error}"
        );
        drop(hook);
        assert_eq!(lua.load("return 6 * 7").eval::<i32>().unwrap(), 42);
    }
}
