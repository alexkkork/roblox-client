//! Cooperative task scheduler for a single local Luau VM. It contains no
//! network, account, executor-injection, or replicated-world behavior.

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
    time::{Duration, Instant},
};

use mlua::{thread::ThreadStatus, Function, Lua, MultiValue, Result, Table, Thread, Value};

#[path = "deadline_interrupt.rs"]
mod deadline_interrupt;

pub type SchedulerRef = Rc<RefCell<Scheduler>>;

const MAX_PENDING: usize = 2048;
const MAX_WAIT_SECONDS: f64 = 86_400.0;
const NONYIELDING_LIMIT: Duration = Duration::from_secs(2);

/// Per-coroutine CPU budget. `total` accumulates active VM time across frames;
/// waits do not consume CPU budget. `slice` bounds each scheduler resumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskPolicy {
    pub slice: Duration,
    pub total: Duration,
    pub resume_across_frames: bool,
}

impl TaskPolicy {
    pub fn new(slice: Duration, total: Duration, resume_across_frames: bool) -> Result<Self> {
        if slice.is_zero() || total.is_zero() {
            return Err(mlua::Error::RuntimeError(
                "task budgets must be positive".into(),
            ));
        }
        Ok(Self {
            slice: slice.min(total),
            total,
            resume_across_frames,
        })
    }
}

impl Default for TaskPolicy {
    fn default() -> Self {
        Self {
            slice: Duration::from_millis(5),
            total: Duration::from_millis(250),
            resume_across_frames: true,
        }
    }
}

/// Per-thread result envelope; tuples remain MLua values and keep trailing
/// nils, tables, functions, and userdata intact until the host consumes them.
#[derive(Debug, Clone)]
pub enum TaskOutcome {
    Completed(MultiValue),
    Yielded(MultiValue),
    Error(String),
    TimedOut(String),
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStateKind {
    Ready,
    Running,
    Sleeping,
    Waiting,
    Completed,
    Error,
    Cancelled,
}

impl TaskStateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Sleeping => "sleeping",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TaskDiagnostics {
    pub state: Option<TaskStateKind>,
    pub resumes: u64,
    pub yields: u64,
    pub slice_interruptions: u64,
    pub execution_ms: f64,
}

/// Point-in-time queue census. This scheduler does not assign work to script
/// owners, so owner attribution is reported as unavailable rather than guessed.
#[derive(Debug, Clone, Default)]
pub struct SchedulerSnapshot {
    pub active_tasks: usize,
    pub pending_tasks: usize,
    pub ready: usize,
    pub sleeping: usize,
    pub waiting: usize,
    pub running: usize,
    pub cancelled: usize,
    pub done: usize,
    pub done_completed: usize,
    pub done_errors: usize,
    pub done_cancelled: usize,
    pub ready_queue_entries: usize,
    pub sleeping_queue_entries: usize,
    pub queue_limit: usize,
    pub active_task_limit: usize,
    pub outcome_records: usize,
    pub outcome_limit: usize,
    pub diagnostic_records: usize,
    pub diagnostic_history_limit: usize,
    pub ready_owner_attribution_available: bool,
}

#[derive(Default)]
pub struct Scheduler {
    ready: VecDeque<Pending>,
    sleeping: Vec<Pending>,
    tasks: HashMap<usize, TaskState>,
    outcomes: HashMap<usize, TaskOutcome>,
    outcome_order: VecDeque<usize>,
    diagnostics: HashMap<usize, TaskDiagnostics>,
    diagnostic_order: VecDeque<usize>,
    running: std::collections::HashSet<usize>,
    next_sequence: u64,
    marker: Option<Table>,
    preempt_marker: Option<Table>,
    errors: VecDeque<String>,
    total_resumes: u64,
    total_preemptions: u64,
    active_policy: Option<TaskPolicy>,
    contexts: Option<Table>,
}

struct Pending {
    thread: Thread,
    args: MultiValue,
    due: Option<Instant>,
    wake: Option<Wake>,
    sequence: u64,
}

enum Wake {
    Wait { started: Instant },
    Delay { started: Instant },
}

struct TaskState {
    cancelled: bool,
    parked: bool,
    uninterrupted: Duration,
    cpu_elapsed: Duration,
    policy: TaskPolicy,
    retain_outcome: bool,
    identity: i64,
    owner: String,
}

impl Default for TaskState {
    fn default() -> Self {
        Self {
            cancelled: false,
            parked: false,
            uninterrupted: Duration::ZERO,
            cpu_elapsed: Duration::ZERO,
            policy: TaskPolicy::default(),
            retain_outcome: false,
            identity: 8,
            owner: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StepReport {
    pub resumed: usize,
    pub completed: usize,
    pub preempted: usize,
    pub sleeping: usize,
    pub waiting: usize,
    pub ready: usize,
    pub errors: Vec<String>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_marker(&mut self, lua: &Lua) -> Result<Table> {
        if let Some(marker) = &self.marker {
            return Ok(marker.clone());
        }
        let marker = lua.create_table()?;
        self.marker = Some(marker.clone());
        Ok(marker)
    }

    fn ensure_contexts(&mut self, lua: &Lua) -> Result<Table> {
        if let Some(contexts) = &self.contexts {
            return Ok(contexts.clone());
        }
        let metatable = lua.create_table()?;
        metatable.set("__mode", "k")?;
        let contexts = lua.create_table()?;
        contexts.set_metatable(Some(metatable))?;
        self.contexts = Some(contexts.clone());
        Ok(contexts)
    }

    fn enqueue(
        &mut self,
        thread: Thread,
        args: MultiValue,
        due: Option<Instant>,
        policy: TaskPolicy,
    ) -> Result<()> {
        let key = thread_key(&thread);
        if self.tasks.len() >= MAX_PENDING && !self.tasks.contains_key(&key) {
            return Err(mlua::Error::RuntimeError(
                "scheduler queue limit reached".into(),
            ));
        }
        self.tasks.entry(key).or_insert_with(|| TaskState {
            policy,
            ..TaskState::default()
        });
        if !self.diagnostics.contains_key(&key) {
            while self.diagnostic_order.len() >= MAX_PENDING {
                if let Some(oldest) = self.diagnostic_order.pop_front() {
                    self.diagnostics.remove(&oldest);
                }
            }
            self.diagnostic_order.push_back(key);
        }
        self.diagnostics.entry(key).or_default().state = Some(if due.is_some() {
            TaskStateKind::Sleeping
        } else {
            TaskStateKind::Ready
        });
        let item = Pending {
            thread,
            args,
            due,
            wake: None,
            sequence: self.next_sequence,
        };
        self.next_sequence = self.next_sequence.wrapping_add(1);
        if due.is_some() {
            self.sleeping.push(item);
        } else {
            self.ready.push_back(item);
        }
        Ok(())
    }

    fn record_error(&mut self, error: String) {
        if self.errors.len() == 64 {
            self.errors.pop_front();
        }
        self.errors.push_back(error);
    }

    fn record_outcome(&mut self, key: usize, outcome: TaskOutcome) {
        if !self.tasks.get(&key).is_some_and(|task| task.retain_outcome) {
            return;
        }
        if !self.outcomes.contains_key(&key) {
            while self.outcome_order.len() >= MAX_PENDING {
                if let Some(oldest) = self.outcome_order.pop_front() {
                    self.outcomes.remove(&oldest);
                }
            }
            self.outcome_order.push_back(key);
        }
        self.outcomes.insert(key, outcome);
    }

    fn publish_outcome(&mut self, key: usize, outcome: TaskOutcome) {
        if self.tasks.get(&key).is_some_and(|task| task.retain_outcome) {
            self.record_outcome(key, outcome);
        }
    }

    fn set_diagnostic_state(&mut self, key: usize, state: TaskStateKind) {
        self.diagnostics.entry(key).or_default().state = Some(state);
    }

    fn snapshot(&self) -> SchedulerSnapshot {
        let mut snapshot = SchedulerSnapshot {
            active_tasks: self.tasks.len(),
            ready: self.ready.len(),
            sleeping: self.sleeping.len(),
            running: self.running.len(),
            cancelled: self.tasks.values().filter(|task| task.cancelled).count(),
            ready_queue_entries: self.ready.len(),
            sleeping_queue_entries: self.sleeping.len(),
            queue_limit: MAX_PENDING,
            active_task_limit: MAX_PENDING,
            outcome_records: self.outcomes.len(),
            outcome_limit: MAX_PENDING,
            diagnostic_records: self.diagnostics.len(),
            diagnostic_history_limit: MAX_PENDING,
            ready_owner_attribution_available: false,
            ..SchedulerSnapshot::default()
        };
        snapshot.waiting = self
            .tasks
            .values()
            .filter(|task| task.parked && !task.cancelled)
            .count();
        snapshot.pending_tasks =
            snapshot.ready + snapshot.sleeping + snapshot.waiting + snapshot.running;
        for diagnostic in self.diagnostics.values() {
            match diagnostic.state {
                Some(TaskStateKind::Completed) => snapshot.done_completed += 1,
                Some(TaskStateKind::Error) => snapshot.done_errors += 1,
                Some(TaskStateKind::Cancelled) => snapshot.done_cancelled += 1,
                _ => {}
            }
        }
        snapshot.done = snapshot.done_completed + snapshot.done_errors + snapshot.done_cancelled;
        snapshot
    }
}

fn thread_key(thread: &Thread) -> usize {
    thread.to_pointer() as usize
}

fn lua_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_integer().map(|number| number as f64))
}

fn current_policy(lua: &Lua) -> TaskPolicy {
    lua.app_data_ref::<SchedulerRef>()
        .map(|scheduler| scheduler.borrow().active_policy.unwrap_or_default())
        .unwrap_or_default()
}

/// Schedule a local function using the active parent policy or the default.
pub fn enqueue(lua: &Lua, function: Function, args: MultiValue) -> Result<Thread> {
    enqueue_inner(lua, function, args, current_policy(lua), false, None, None)
}

/// Schedule a local function with a per-thread execution budget.
pub fn enqueue_with_policy(
    lua: &Lua,
    function: Function,
    args: MultiValue,
    policy: TaskPolicy,
) -> Result<Thread> {
    enqueue_inner(lua, function, args, policy, true, None, None)
}

/// Enqueue a source-owned script. Native host semantics assign identity 2 when
/// a thread first receives a `script:` owner; this is metadata only and grants
/// no engine or server permission.
pub fn enqueue_owned(
    lua: &Lua,
    function: Function,
    args: MultiValue,
    owner: String,
) -> Result<Thread> {
    enqueue_inner(
        lua,
        function,
        args,
        current_policy(lua),
        false,
        Some(owner),
        None,
    )
}

/// Enqueue a callback while inheriting its creator coroutine's local metadata.
pub fn enqueue_from(
    lua: &Lua,
    function: Function,
    args: MultiValue,
    creator: Option<&Thread>,
) -> Result<Thread> {
    enqueue_inner(
        lua,
        function,
        args,
        current_policy(lua),
        false,
        None,
        creator,
    )
}

fn enqueue_inner(
    lua: &Lua,
    function: Function,
    args: MultiValue,
    policy: TaskPolicy,
    retain_outcome: bool,
    owner: Option<String>,
    creator: Option<&Thread>,
) -> Result<Thread> {
    if policy.slice.is_zero() || policy.total.is_zero() {
        return Err(mlua::Error::RuntimeError(
            "task budgets must be positive".into(),
        ));
    }
    let policy = TaskPolicy {
        slice: policy.slice.min(policy.total),
        ..policy
    };
    let thread = lua.create_thread(function)?;
    let scheduler = lua
        .app_data_ref::<SchedulerRef>()
        .ok_or_else(|| mlua::Error::RuntimeError("scheduler is not installed".into()))?
        .clone();
    {
        let mut state = scheduler.borrow_mut();
        let parent_thread = creator.cloned().unwrap_or_else(|| lua.current_thread());
        let parent_context = state.contexts.as_ref().and_then(|contexts| {
            contexts
                .get::<Option<Table>>(parent_thread.clone())
                .ok()
                .flatten()
        });
        let inherited_identity = parent_context
            .as_ref()
            .and_then(|ctx| ctx.get::<i64>("identity").ok())
            .unwrap_or(8);
        let inherited_owner = parent_context
            .as_ref()
            .and_then(|ctx| ctx.get::<String>("owner").ok())
            .unwrap_or_default();
        let child_owner = owner.unwrap_or(inherited_owner);
        let child_identity = if child_owner.starts_with("script:")
            && child_owner
                != parent_context
                    .as_ref()
                    .and_then(|ctx| ctx.get::<String>("owner").ok())
                    .unwrap_or_default()
        {
            2
        } else {
            inherited_identity
        };
        state.enqueue(thread.clone(), args, None, policy)?;
        let contexts = state.ensure_contexts(lua)?;
        let context = lua.create_table()?;
        context.set("identity", child_identity)?;
        context.set("owner", child_owner.clone())?;
        contexts.set(thread.clone(), context)?;
        if let Some(task) = state.tasks.get_mut(&thread_key(&thread)) {
            task.retain_outcome = retain_outcome;
            task.identity = child_identity;
            task.owner = child_owner;
        }
    }
    Ok(thread)
}

/// Returns the current scheduler-tracked coroutine's identity. Untracked host
/// and direct `coroutine.create` threads use the native default identity 8.
pub fn thread_identity(lua: &Lua) -> i64 {
    let current = lua.current_thread();
    current_context(lua, &current)
        .and_then(|context| context.get::<i64>("identity").ok())
        .unwrap_or(8)
}

pub fn set_thread_identity(lua: &Lua, identity: i64) -> Result<()> {
    let current = lua.current_thread();
    let scheduler = lua
        .app_data_ref::<SchedulerRef>()
        .ok_or_else(|| mlua::Error::RuntimeError("thread security context unavailable".into()))?
        .clone();
    let mut state = scheduler.borrow_mut();
    let contexts = state
        .contexts
        .clone()
        .ok_or_else(|| mlua::Error::RuntimeError("thread security context unavailable".into()))?;
    let context: Option<Table> = contexts.get(current.clone())?;
    let context = context
        .ok_or_else(|| mlua::Error::RuntimeError("thread security context unavailable".into()))?;
    context.set("identity", identity)?;
    if let Some(task) = state.tasks.get_mut(&thread_key(&current)) {
        task.identity = identity;
    }
    Ok(())
}

fn current_context(lua: &Lua, thread: &Thread) -> Option<Table> {
    let scheduler = lua.app_data_ref::<SchedulerRef>()?.clone();
    let contexts = scheduler.borrow().contexts.clone()?;
    contexts.get(thread.clone()).ok().flatten()
}

fn install_thread_context(
    lua: &Lua,
    scheduler: &SchedulerRef,
    thread: &Thread,
    identity: i64,
    owner: String,
) -> Result<()> {
    let contexts = scheduler.borrow_mut().ensure_contexts(lua)?;
    let context = lua.create_table()?;
    context.set("identity", identity)?;
    context.set("owner", owner)?;
    contexts.set(thread.clone(), context)?;
    Ok(())
}

/// Resume a suspended thread with the exact supplied tuple. No scheduler
/// borrow is held while Lua executes.
pub fn resume(lua: &Lua, thread: Thread, args: MultiValue) -> Result<()> {
    if !thread.is_resumable() {
        return Ok(());
    }
    let scheduler = lua
        .app_data_ref::<SchedulerRef>()
        .ok_or_else(|| mlua::Error::RuntimeError("scheduler is not installed".into()))?
        .clone();
    let key = thread_key(&thread);
    let policy = {
        let mut state = scheduler.borrow_mut();
        state.ready.retain(|item| thread_key(&item.thread) != key);
        state
            .sleeping
            .retain(|item| thread_key(&item.thread) != key);
        let task = state.tasks.get_mut(&key);
        if let Some(task) = task {
            task.parked = false;
            task.policy
        } else {
            state.active_policy.unwrap_or_default()
        }
    };
    let result = scheduler.borrow_mut().enqueue(thread, args, None, policy);
    result
}

/// Park a suspended coroutine until an external local event calls `resume`.
pub fn park(lua: &Lua, thread: &Thread) -> Result<()> {
    let scheduler = lua
        .app_data_ref::<SchedulerRef>()
        .ok_or_else(|| mlua::Error::RuntimeError("scheduler is not installed".into()))?
        .clone();
    let mut state = scheduler.borrow_mut();
    let key = thread_key(thread);
    state.ready.retain(|item| thread_key(&item.thread) != key);
    state
        .sleeping
        .retain(|item| thread_key(&item.thread) != key);
    state.tasks.entry(key).or_default().parked = true;
    state.set_diagnostic_state(key, TaskStateKind::Waiting);
    Ok(())
}

pub fn is_parked(scheduler: &SchedulerRef, thread: &Thread) -> bool {
    scheduler
        .borrow()
        .tasks
        .get(&thread_key(thread))
        .is_some_and(|task| task.parked)
}

pub fn take_yielded(scheduler: &SchedulerRef, thread: &Thread) -> Option<MultiValue> {
    match take_outcome(scheduler, thread)? {
        TaskOutcome::Yielded(values) => Some(values),
        other => {
            scheduler
                .borrow_mut()
                .record_outcome(thread_key(thread), other);
            None
        }
    }
}

pub fn take_outcome(scheduler: &SchedulerRef, thread: &Thread) -> Option<TaskOutcome> {
    let key = thread_key(thread);
    let mut state = scheduler.borrow_mut();
    let outcome = state.outcomes.remove(&key);
    if outcome.is_some() {
        state.outcome_order.retain(|item| *item != key);
    }
    outcome
}

pub fn diagnostics(scheduler: &SchedulerRef, thread: &Thread) -> Option<TaskDiagnostics> {
    scheduler
        .borrow()
        .diagnostics
        .get(&thread_key(thread))
        .cloned()
}

pub fn snapshot(scheduler: &SchedulerRef) -> SchedulerSnapshot {
    scheduler.borrow().snapshot()
}

pub fn drain_errors(scheduler: &SchedulerRef) -> Vec<String> {
    scheduler.borrow_mut().errors.drain(..).collect()
}

/// Install the standard task helpers in this local Luau state.
pub fn install(lua: &Lua, scheduler: &SchedulerRef) -> Result<()> {
    lua.set_app_data(scheduler.clone());
    {
        let mut state = scheduler.borrow_mut();
        state.ensure_contexts(lua)?;
        state.preempt_marker = Some(deadline_interrupt::marker(lua)?);
    }
    let task = lua.create_table()?;
    let marker = scheduler.borrow_mut().ensure_marker(lua)?;

    let spawn = lua.create_function(|lua, (function, args): (Function, MultiValue)| {
        enqueue(lua, function, args)
    })?;
    task.set("spawn", spawn.clone())?;
    task.set("defer", spawn)?;

    let delay = lua.create_function(
        |lua, (seconds, function, args): (f64, Function, MultiValue)| {
            if !seconds.is_finite() || !(0.0..=MAX_WAIT_SECONDS).contains(&seconds) {
                return Err(mlua::Error::RuntimeError(
                    "invalid task.delay duration".into(),
                ));
            }
            let thread = lua.create_thread(function)?;
            let scheduler = lua
                .app_data_ref::<SchedulerRef>()
                .ok_or_else(|| mlua::Error::RuntimeError("scheduler is not installed".into()))?
                .clone();
            let started = Instant::now();
            let policy = current_policy(lua);
            let parent = lua.current_thread();
            let parent_context = current_context(lua, &parent);
            let identity = parent_context
                .as_ref()
                .and_then(|ctx| ctx.get::<i64>("identity").ok())
                .unwrap_or(8);
            let owner = parent_context
                .as_ref()
                .and_then(|ctx| ctx.get::<String>("owner").ok())
                .unwrap_or_default();
            install_thread_context(lua, &scheduler, &thread, identity, owner)?;
            scheduler.borrow_mut().enqueue(
                thread.clone(),
                args,
                Some(started + Duration::from_secs_f64(seconds)),
                policy,
            )?;
            if let Some(item) = scheduler.borrow_mut().sleeping.last_mut() {
                item.wake = Some(Wake::Delay { started });
            }
            Ok(thread)
        },
    )?;
    task.set("delay", delay)?;

    let wait_setup: Function = lua.load(
        "return function(task, marker) task.wait = function(seconds)\n            seconds = seconds or 0\n            if type(seconds) ~= 'number' or seconds ~= seconds or seconds < 0 or seconds > 86400 then error('invalid task.wait duration', 2) end\n            return coroutine.yield(marker, seconds)\n        end end",
    ).eval()?;
    wait_setup.call::<()>((task.clone(), marker))?;

    let cancel = lua.create_function(|lua, thread: Thread| {
        if let Some(scheduler) = lua.app_data_ref::<SchedulerRef>() {
            let scheduler = scheduler.clone();
            let key = thread_key(&thread);
            if thread.is_running() {
                if let Some(task) = scheduler.borrow_mut().tasks.get_mut(&key) {
                    task.cancelled = true;
                }
            } else {
                let mut state = scheduler.borrow_mut();
                state.ready.retain(|item| thread_key(&item.thread) != key);
                state
                    .sleeping
                    .retain(|item| thread_key(&item.thread) != key);
                state.publish_outcome(key, TaskOutcome::Cancelled);
                state.tasks.remove(&key);
                state.set_diagnostic_state(key, TaskStateKind::Cancelled);
            }
        }
        Ok(())
    })?;
    task.set("cancel", cancel)?;
    lua.globals().set("task", task)?;

    for name in ["getthreadidentity", "getidentity", "getthreadcontext"] {
        lua.globals().set(
            name,
            lua.create_function(|lua, ()| Ok(thread_identity(lua)))?,
        )?;
    }
    for name in ["setthreadidentity", "setidentity", "setthreadcontext"] {
        lua.globals().set(
            name,
            lua.create_function(|lua, identity: i64| set_thread_identity(lua, identity))?,
        )?;
    }
    let inherit = lua.create_function(|lua, child: Thread| {
        let scheduler = lua
            .app_data_ref::<SchedulerRef>()
            .ok_or_else(|| mlua::Error::RuntimeError("scheduler is not installed".into()))?
            .clone();
        let parent = lua.current_thread();
        let parent_context = current_context(lua, &parent);
        let identity = parent_context
            .as_ref()
            .and_then(|ctx| ctx.get::<i64>("identity").ok())
            .unwrap_or(8);
        let owner = parent_context
            .as_ref()
            .and_then(|ctx| ctx.get::<String>("owner").ok())
            .unwrap_or_default();
        install_thread_context(lua, &scheduler, &child, identity, owner)
    })?;
    let coroutine = lua.globals().get::<Table>("coroutine")?;
    let current_preempt_marker = lua.create_function(|lua, ()| {
        Ok(lua
            .named_registry_value::<Table>(deadline_interrupt::MARKER_KEY)
            .ok())
    })?;
    let wrap_coroutines: Function = lua
        .load(
            r#"return function(co, native_create, native_resume, inherit, current_preempt_marker)
                co.create = function(fn) local thread = native_create(fn); inherit(thread); return thread end
                co.resume = function(thread, ...)
                    local args = table.pack(...)
                    while true do
                        local result = table.pack(native_resume(thread, table.unpack(args, 1, args.n)))
                        local marker = current_preempt_marker()
                        if result[1] and marker ~= nil and result[2] == marker then
                            coroutine.yield(marker)
                            args = table.pack()
                        else
                            return table.unpack(result, 1, result.n)
                        end
                    end
                end
                co.wrap = function(fn)
                    local thread = co.create(fn)
                    return function(...)
                        local result = table.pack(co.resume(thread, ...))
                        if not result[1] then error(result[2], 2) end
                        return table.unpack(result, 2, result.n)
                    end
                end
            end"#,
        )
        .eval()?;
    wrap_coroutines.call::<()>((
        coroutine.clone(),
        coroutine.get::<Function>("create")?,
        coroutine.get::<Function>("resume")?,
        inherit,
        current_preempt_marker,
    ))?;
    Ok(())
}

/// Execute at most one host budget of VM work. Per-task slices can preempt at
/// 5ms and resume on a later call when their policy permits it.
pub fn step(lua: &Lua, scheduler: &SchedulerRef, now: Instant, budget: Duration) -> StepReport {
    step_with_limits(lua, scheduler, now, budget)
}

pub fn step_with_limits(
    lua: &Lua,
    scheduler: &SchedulerRef,
    now: Instant,
    budget: Duration,
) -> StepReport {
    let budget = budget.min(NONYIELDING_LIMIT);
    let host_deadline = Instant::now() + budget;
    let mut report = StepReport::default();

    {
        let mut state = scheduler.borrow_mut();
        let mut ready_sleepers = Vec::new();
        let mut still_sleeping = Vec::with_capacity(state.sleeping.len());
        for item in state.sleeping.drain(..) {
            if item.due.is_some_and(|due| due <= now) {
                ready_sleepers.push(item);
            } else {
                still_sleeping.push(item);
            }
        }
        state.sleeping = still_sleeping;
        ready_sleepers.sort_by_key(|item| (item.due, item.sequence));
        for mut item in ready_sleepers {
            if let Some(wake) = item.wake.take() {
                let (started, is_wait) = match wake {
                    Wake::Wait { started } => (started, true),
                    Wake::Delay { started } => (started, false),
                };
                let elapsed = now.saturating_duration_since(started).as_secs_f64();
                if is_wait {
                    item.args = MultiValue::from_vec(vec![Value::Number(elapsed.into())]);
                } else {
                    let mut args = vec![Value::Number(elapsed.into())];
                    args.extend(item.args);
                    item.args = MultiValue::from_vec(args);
                }
            }
            state.set_diagnostic_state(thread_key(&item.thread), TaskStateKind::Ready);
            state.ready.push_back(item);
        }
    }

    while Instant::now() < host_deadline {
        let Some(mut item) = scheduler.borrow_mut().ready.pop_front() else {
            break;
        };
        let key = thread_key(&item.thread);
        let (policy, cpu_elapsed, uninterrupted, cancelled) = {
            let state = scheduler.borrow();
            state
                .tasks
                .get(&key)
                .map(|task| {
                    (
                        task.policy,
                        task.cpu_elapsed,
                        task.uninterrupted,
                        task.cancelled,
                    )
                })
                .unwrap_or((TaskPolicy::default(), Duration::ZERO, Duration::ZERO, false))
        };
        if cancelled {
            let mut state = scheduler.borrow_mut();
            state.set_diagnostic_state(key, TaskStateKind::Cancelled);
            state.publish_outcome(key, TaskOutcome::Cancelled);
            state.tasks.remove(&key);
            report.completed += 1;
            continue;
        }
        let remaining = policy.total.saturating_sub(cpu_elapsed);
        if remaining.is_zero() {
            let mut state = scheduler.borrow_mut();
            let message = "local Luau task exceeded its execution budget".to_owned();
            state.set_diagnostic_state(key, TaskStateKind::Error);
            state.record_error(message.clone());
            state.publish_outcome(key, TaskOutcome::TimedOut(message.clone()));
            state.tasks.remove(&key);
            report.errors.push(message);
            report.completed += 1;
            continue;
        }

        let started = Instant::now();
        let task_deadline = started + policy.slice.min(remaining);
        let resume_deadline = host_deadline.min(task_deadline);
        let hard_limit = if policy.resume_across_frames {
            remaining.min(NONYIELDING_LIMIT.saturating_sub(uninterrupted))
        } else {
            policy.slice.min(remaining)
        };
        let hard_deadline = if policy.resume_across_frames {
            started + hard_limit
        } else {
            resume_deadline
        };
        let interrupt =
            match deadline_interrupt::DeadlineInterrupt::new(lua, resume_deadline, hard_deadline) {
                Ok(interrupt) => interrupt,
                Err(error) => {
                    let message = error.to_string();
                    let mut state = scheduler.borrow_mut();
                    state.set_diagnostic_state(key, TaskStateKind::Error);
                    state.record_error(message.clone());
                    state.publish_outcome(key, TaskOutcome::Error(message.clone()));
                    state.tasks.remove(&key);
                    report.errors.push(message);
                    report.completed += 1;
                    continue;
                }
            };

        let previous_policy = scheduler.borrow().active_policy;
        {
            let mut state = scheduler.borrow_mut();
            state.running.insert(key);
            state.active_policy = Some(policy);
            state.set_diagnostic_state(key, TaskStateKind::Running);
            let diagnostic = state.diagnostics.entry(key).or_default();
            diagnostic.resumes = diagnostic.resumes.saturating_add(1);
        }
        let result = item.thread.resume::<MultiValue>(item.args);
        let was_preempted = interrupt.preempted();
        let was_aborted = interrupt.aborted();
        drop(interrupt);
        {
            let mut state = scheduler.borrow_mut();
            state.running.remove(&key);
            state.active_policy = previous_policy;
        }
        let elapsed = started.elapsed();
        report.resumed += 1;

        let mut state = scheduler.borrow_mut();
        state.total_resumes = state.total_resumes.saturating_add(1);
        if state.tasks.get(&key).is_some_and(|task| task.cancelled) {
            state.set_diagnostic_state(key, TaskStateKind::Cancelled);
            state.diagnostics.entry(key).or_default().execution_ms +=
                elapsed.as_secs_f64() * 1000.0;
            state.publish_outcome(key, TaskOutcome::Cancelled);
            state.tasks.remove(&key);
            report.completed += 1;
            continue;
        }
        if was_aborted {
            // Even if the script caught the native callback's timeout, this
            // resume exhausted its budget and cannot be safely replayed.
            let message =
                "local Luau task exceeded its execution budget in a non-yieldable callback"
                    .to_owned();
            state.record_error(message.clone());
            let diagnostic = state.diagnostics.entry(key).or_default();
            diagnostic.execution_ms += elapsed.as_secs_f64() * 1000.0;
            diagnostic.slice_interruptions = diagnostic.slice_interruptions.saturating_add(1);
            state.total_preemptions = state.total_preemptions.saturating_add(1);
            state.set_diagnostic_state(key, TaskStateKind::Error);
            state.publish_outcome(key, TaskOutcome::TimedOut(message.clone()));
            state.tasks.remove(&key);
            report.errors.push(message);
            report.preempted += 1;
            report.completed += 1;
            continue;
        }
        let output = match result {
            Ok(values) => values,
            Err(error) => {
                let message = error.to_string();
                state.record_error(message.clone());
                state.diagnostics.entry(key).or_default().execution_ms +=
                    elapsed.as_secs_f64() * 1000.0;
                state.set_diagnostic_state(key, TaskStateKind::Error);
                state.publish_outcome(key, TaskOutcome::Error(message.clone()));
                report.errors.push(message);
                state.tasks.remove(&key);
                report.completed += 1;
                continue;
            }
        };
        if item.thread.status() == ThreadStatus::Finished {
            state.set_diagnostic_state(key, TaskStateKind::Completed);
            state.diagnostics.entry(key).or_default().execution_ms +=
                elapsed.as_secs_f64() * 1000.0;
            state.publish_outcome(key, TaskOutcome::Completed(output));
            state.tasks.remove(&key);
            report.completed += 1;
            continue;
        }

        let propagated_preemption = output.front().is_some_and(|value| {
            matches!(value, Value::Table(table) if state.preempt_marker.as_ref().is_some_and(|marker| marker.to_pointer() == table.to_pointer()))
        });
        if was_preempted || propagated_preemption {
            let (uninterrupted, cpu_elapsed) = {
                let task = state.tasks.entry(key).or_default();
                task.uninterrupted = task.uninterrupted.saturating_add(elapsed);
                task.cpu_elapsed = task.cpu_elapsed.saturating_add(elapsed);
                (task.uninterrupted, task.cpu_elapsed)
            };
            {
                let diagnostic = state.diagnostics.entry(key).or_default();
                diagnostic.execution_ms += elapsed.as_secs_f64() * 1000.0;
                diagnostic.slice_interruptions = diagnostic.slice_interruptions.saturating_add(1);
            }
            state.total_preemptions = state.total_preemptions.saturating_add(1);
            report.preempted += 1;
            if uninterrupted >= NONYIELDING_LIMIT
                || cpu_elapsed >= policy.total
                || !policy.resume_across_frames
            {
                let message = if uninterrupted >= NONYIELDING_LIMIT {
                    "local Luau task exceeded the 2 second non-yielding limit"
                } else {
                    "local Luau task exceeded its execution budget"
                }
                .to_owned();
                state.record_error(message.clone());
                state.set_diagnostic_state(key, TaskStateKind::Error);
                state.publish_outcome(key, TaskOutcome::TimedOut(message.clone()));
                report.errors.push(message);
                state.tasks.remove(&key);
                report.completed += 1;
                continue;
            }
            state.set_diagnostic_state(key, TaskStateKind::Ready);
            item.args = MultiValue::new();
            item.due = None;
            state.ready.push_back(item);
            continue;
        }

        let parked = {
            let task = state.tasks.entry(key).or_default();
            task.uninterrupted = Duration::ZERO;
            task.cpu_elapsed = task.cpu_elapsed.saturating_add(elapsed);
            task.parked
        };
        state.diagnostics.entry(key).or_default().execution_ms += elapsed.as_secs_f64() * 1000.0;
        if parked {
            state.tasks.entry(key).or_default().cpu_elapsed = Duration::ZERO;
            let diagnostic = state.diagnostics.entry(key).or_default();
            diagnostic.yields = diagnostic.yields.saturating_add(1);
            state.set_diagnostic_state(key, TaskStateKind::Waiting);
            continue;
        }

        let values: Vec<Value> = output.into_iter().collect();
        let marker = state.marker.as_ref().map(Table::to_pointer);
        let wait_seconds = match (values.first(), marker) {
            (Some(Value::Table(value)), Some(marker)) if value.to_pointer() == marker => {
                values.get(1).and_then(lua_number)
            }
            _ => None,
        };
        if let Some(seconds) = wait_seconds {
            if seconds.is_finite() && (0.0..=MAX_WAIT_SECONDS).contains(&seconds) {
                let started = Instant::now();
                item.args = MultiValue::new();
                item.due = Some(started + Duration::from_secs_f64(seconds));
                item.wake = Some(Wake::Wait { started });
                item.sequence = state.next_sequence;
                state.next_sequence = state.next_sequence.wrapping_add(1);
                let diagnostic = state.diagnostics.entry(key).or_default();
                diagnostic.yields = diagnostic.yields.saturating_add(1);
                state.tasks.entry(key).or_default().cpu_elapsed = Duration::ZERO;
                state.set_diagnostic_state(key, TaskStateKind::Sleeping);
                state.sleeping.push(item);
            } else {
                let message = "invalid task.wait duration".to_owned();
                state.record_error(message.clone());
                state.set_diagnostic_state(key, TaskStateKind::Error);
                state.publish_outcome(key, TaskOutcome::Error(message.clone()));
                report.errors.push(message);
                state.tasks.remove(&key);
                report.completed += 1;
            }
        } else if values.first().is_some_and(
            |value| matches!(value, Value::Table(table) if Some(table.to_pointer()) == marker),
        ) {
            let message = "invalid task.wait duration".to_owned();
            state.record_error(message.clone());
            state.set_diagnostic_state(key, TaskStateKind::Error);
            state.publish_outcome(key, TaskOutcome::Error(message.clone()));
            report.errors.push(message);
            state.tasks.remove(&key);
            report.completed += 1;
        } else {
            let yielded_values = MultiValue::from_vec(values);
            let task = state.tasks.entry(key).or_default();
            task.parked = true;
            task.cpu_elapsed = Duration::ZERO;
            state.publish_outcome(key, TaskOutcome::Yielded(yielded_values));
            let diagnostic = state.diagnostics.entry(key).or_default();
            diagnostic.yields = diagnostic.yields.saturating_add(1);
            state.set_diagnostic_state(key, TaskStateKind::Waiting);
        }
        if report.resumed >= MAX_PENDING {
            break;
        }
    }

    let state = scheduler.borrow();
    report.ready = state.ready.len();
    report.sleeping = state.sleeping.len();
    report.waiting = state
        .tasks
        .values()
        .filter(|task| task.parked && !task.cancelled)
        .count();
    report
}
