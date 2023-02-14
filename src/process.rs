//! [`ByteCode`] execution utilities.
//! 
//! Because the NetsBlox runtime allows for the creation of cycles, all program-accessible objects must be garbage collected.
//! To do this, we use the `gc-arena` crate, which allows a simple and correct mechanism for provably disjoint garbage collected runtime environments.
//! However, `gc-arena` makes this guarantee by forbidding garbage collected objects from leaving the arena.
//! Thus, many types in this module, such as  [`Value`] and [`Process`], are branded with an invariant `'gc` lifetime and can only be access by callback.
//! Some utilities are provided to export these values from the runtime environment if needed.

use std::prelude::v1::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque, vec_deque::Iter as VecDequeIter};
use std::iter::{self, Cycle};
use std::cmp::Ordering;
use std::rc::Rc;

#[cfg(feature = "serde")]
use serde::Serialize;

use crate::*;
use crate::gc::*;
use crate::json::*;
use crate::runtime::*;
use crate::bytecode::*;

fn empty_string() -> Rc<String> {
    #[cfg(feature = "std")]
    {
        std::thread_local! {
            static VALUE: Rc<String> = Rc::new(String::new());
        }
        VALUE.with(|x| x.clone())
    }
    #[cfg(not(feature = "std"))]
    {
        Rc::new(String::new())
    }
}

/// A variable entry in the structure expected by the standard js extension.
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct VarEntry {
    pub name: String,
    pub value: String,
}
/// A trace entry in the structure expected by the standard js extension.
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct TraceEntry {
    pub location: String,
    pub locals: Vec<VarEntry>,
}
/// A error message in the structure expected by the standard js extension.
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct ErrorSummary {
    pub cause: String,
    pub entity: String,
    pub globals: Vec<VarEntry>,
    pub fields: Vec<VarEntry>,
    pub trace: Vec<TraceEntry>,
}
impl ErrorSummary {
    pub fn extract<S: System>(error: &ExecError<S>, process: &Process<S>, locations: &Locations) -> Self {
        let raw_entity = process.get_entity();
        let entity = raw_entity.read().name.clone();
        let cause = format!("{:?}", error.cause);

        fn summarize_symbols<S: System>(symbols: &SymbolTable<'_, S>) -> Vec<VarEntry> {
            let mut res = Vec::with_capacity(symbols.len());
            for (k, v) in symbols {
                res.push(VarEntry { name: k.clone(), value: format!("{:?}", &*v.get()) });
            }
            res
        }
        let globals = summarize_symbols(&process.get_global_context().read().globals);
        let fields = summarize_symbols(&raw_entity.read().fields);

        let call_stack = process.get_call_stack();
        let mut trace = Vec::with_capacity(call_stack.len());
        for (pos, locals) in iter::zip(call_stack[1..].iter().map(|x| x.called_from).chain(iter::once(error.pos)), call_stack.iter().map(|x| &x.locals)) {
            if let Some(loc) = locations.lookup(pos) {
                trace.push(TraceEntry { location: loc.clone(), locals: summarize_symbols(locals) });
            }
        }
        debug_assert_eq!(trace.len(), call_stack.len());

        Self { entity, cause, globals, fields, trace }
    }
}

/// An execution error from a [`Process`] (see [`Process::step`]).
///
/// This consists of an [`ErrorCause`] value describing the cause, as well as the bytecode location of the error.
/// By using the [`Locations`] information from [`ByteCode::compile`], it is possible to determine
/// a human-readable error location in the original program.
#[derive(Educe)]
#[educe(Debug)]
pub struct ExecError<S: System> {
    pub cause: ErrorCause<S>,
    pub pos: usize,
}

/// Result of stepping through a [`Process`].
pub enum ProcessStep<'gc, S: System> {
    /// The process was not running.
    Idle,
    /// The process executed an instruction successfully and does not need to yield.
    Normal,
    /// The process has signaled a yield point so that other code can run.
    /// Many yield results may occur back-to-back, such as while awaiting an asynchronous result.
    /// 
    /// Yielding is needed for executing an entire project's scripts so that they can appear to run simultaneously.
    /// If instead you are explicitly only using a single sandboxed process, this can be treated equivalently to [`ProcessStep::Normal`].
    Yield,
    /// The process has successfully terminated with the given return value, or [`None`] if terminated by an (error-less) abort,
    /// such as a stop script command or the death of the process's associated entity.
    Terminate { result: Option<Value<'gc, S>> },
    /// The process has requested to broadcast a message to all entities, which may trigger other code to execute.
    Broadcast { msg_type: String, barrier: Option<Barrier> },
}

/// An entry in the call stack of a [`Process`].
/// 
/// This contains information about the call origin and local variables defined in the called context.
#[derive(Collect)]
#[collect(no_drop, bound = "")]
pub struct CallStackEntry<'gc, S: System> {
    #[collect(require_static)] pub called_from: usize,
    #[collect(require_static)]     return_to: usize,
                               pub locals: SymbolTable<'gc, S>,

    #[collect(require_static)] warp_counter: usize,
    #[collect(require_static)] value_stack_size: usize,
    #[collect(require_static)] handler_stack_size: usize,
}

struct Handler {
    pos: usize,
    var: String,
    warp_counter: usize,
    call_stack_size: usize,
    value_stack_size: usize,
}

enum Defer<S: System> {
    Request { key: S::RequestKey, aft_pos: usize, action: RequestAction },
    Command { key: S::CommandKey, aft_pos: usize },
    MessageReply { key: S::ExternReplyKey, aft_pos: usize },
    Barrier { condition: BarrierCondition, aft_pos: usize },
    Sleep { until: u64, aft_pos: usize },
}
enum RequestAction {
    Rpc, Syscall, Input, Push,
}

/// A [`ByteCode`] execution primitive.
/// 
/// A [`Process`] is a self-contained thread of execution.
/// It maintains its own state machine for executing instructions step by step.
#[derive(Collect)]
#[collect(no_drop, bound = "")]
pub struct Process<'gc, S: System> {
                               global_context: GcCell<'gc, GlobalContext<'gc, S>>,
                               entity: GcCell<'gc, Entity<'gc, S>>,
    #[collect(require_static)] start_pos: usize,
    #[collect(require_static)] pos: usize,
    #[collect(require_static)] running: bool,
    #[collect(require_static)] barrier: Option<Barrier>,
    #[collect(require_static)] reply_key: Option<S::InternReplyKey>,
    #[collect(require_static)] warp_counter: usize,
                               call_stack: Vec<CallStackEntry<'gc, S>>,
                               value_stack: Vec<Value<'gc, S>>,
    #[collect(require_static)] handler_stack: Vec<Handler>,
    #[collect(require_static)] meta_stack: Vec<String>,
    #[collect(require_static)] defer: Option<Defer<S>>,
                               last_syscall_error: Option<Value<'gc, S>>,
                               last_rpc_error: Option<Value<'gc, S>>,
                               last_answer: Option<Value<'gc, S>>,
}
impl<'gc, S: System> Process<'gc, S> {
    /// Creates a new [`Process`] that is tied to a given `start_pos` (entry point) in the [`ByteCode`] and associated with the specified `entity` and `system`.
    /// The created process is initialized to an idle (non-running) state; use [`Process::initialize`] to begin execution.
    pub fn new(global_context: GcCell<'gc, GlobalContext<'gc, S>>, entity: GcCell<'gc, Entity<'gc, S>>, start_pos: usize) -> Self {
        Self {
            global_context, entity, start_pos,
            running: false,
            barrier: None,
            reply_key: None,
            pos: 0,
            warp_counter: 0,
            call_stack: vec![],
            value_stack: vec![],
            handler_stack: vec![],
            meta_stack: vec![],
            defer: None,
            last_syscall_error: None,
            last_rpc_error: None,
            last_answer: None,
        }
    }
    /// Checks if the process is currently running.
    /// Note that the process will not run on its own (see [`Process::step`]).
    pub fn is_running(&self) -> bool {
        self.running
    }
    /// Gets the global context that this process is tied to (see [`Process::new`]).
    pub fn get_global_context(&self) -> GcCell<'gc, GlobalContext<'gc, S>> {
        self.global_context
    }
    /// Gets the entity that this process is tied to (see [`Process::new`]).
    pub fn get_entity(&self) -> GcCell<'gc, Entity<'gc, S>> {
        self.entity
    }
    /// Gets a reference to the current call stack.
    /// This gives access to stack trace information including all local scopes in the call chain.
    /// Note that the call stack is never empty, and that the always-present first element (denoting the initial execution request) is a special
    /// entry which has an invalid value for [`CallStackEntry::called_from`], namely [`usize::MAX`].
    pub fn get_call_stack(&self) -> &[CallStackEntry<'gc, S>] {
        &self.call_stack
    }
    /// Prepares the process to execute starting at the main entry point (see [`Process::new`]) with the provided input local variables.
    /// A [`Barrier`] may also be set, which will be destroyed upon termination, either due to completion or an error.
    /// 
    /// Any previous process state is wiped when performing this action.
    pub fn initialize(&mut self, locals: SymbolTable<'gc, S>, barrier: Option<Barrier>, reply_key: Option<S::InternReplyKey>) {
        self.pos = self.start_pos;
        self.running = true;
        self.barrier = barrier;
        self.reply_key = reply_key;
        self.warp_counter = 0;
        self.call_stack.clear();
        self.call_stack.push(CallStackEntry {
            called_from: usize::MAX,
            return_to: usize::MAX,
            warp_counter: 0,
            value_stack_size: 0,
            handler_stack_size: 0,
            locals,
        });
        self.value_stack.clear();
        self.handler_stack.clear();
        self.meta_stack.clear();
        self.defer = None;
        self.last_syscall_error = None;
        self.last_rpc_error = None;
        self.last_answer = None;
    }
    /// Executes a single bytecode instruction.
    /// The return value can be used to determine what additional effects the script has requested,
    /// as well as to retrieve the return value or execution error in the event that the process terminates.
    /// 
    /// The process transitions to the idle state (see [`Process::is_running`]) upon failing with [`Err`] or succeeding with [`ProcessStep::Terminate`].
    pub fn step(&mut self, mc: MutationContext<'gc, '_>) -> Result<ProcessStep<'gc, S>, ExecError<S>> {
        let mut res = self.step_impl(mc);
        if let Err(err) = &res {
            if let Some(Handler { pos, var, warp_counter, call_stack_size, value_stack_size }) = self.handler_stack.last() {
                self.warp_counter = *warp_counter;
                self.call_stack.drain(*call_stack_size..);
                self.value_stack.drain(*value_stack_size..);
                debug_assert_eq!(self.call_stack.len(), *call_stack_size);
                debug_assert_eq!(self.value_stack.len(), *value_stack_size);

                let msg = match err {
                    ErrorCause::Custom { msg } => msg.clone(),
                    _ => format!("{err:?}"),
                };
                self.call_stack.last_mut().unwrap().locals.redefine_or_define(var, Shared::Unique(Value::String(Rc::new(msg))));
                self.pos = *pos;
                res = Ok(ProcessStep::Normal);
            }
        }

        if let Ok(ProcessStep::Terminate { .. }) | Err(_) = &res {
            self.running = false;
            self.barrier = None;
            self.reply_key = None;
        }
        res.map_err(|cause| ExecError { cause, pos: self.pos })
    }
    fn step_impl(&mut self, mc: MutationContext<'gc, '_>) -> Result<ProcessStep<'gc, S>, ErrorCause<S>> {
        let mut global_context = self.global_context.write(mc);
        let mut global_context = &mut *global_context;

        fn process_result<'gc, S: System, T>(result: Result<T, String>, error_scheme: ErrorScheme, stack: Option<&mut Vec<Value<'gc, S>>>, last_ok: Option<&mut Option<Value<'gc, S>>>, last_err: Option<&mut Option<Value<'gc, S>>>, to_value: fn(T) -> Option<Value<'gc, S>>) -> Result<(), ErrorCause<S>> {
            match result {
                Ok(x) => match to_value(x) {
                    Some(x) => {
                        if let Some(last_err) = last_err { *last_err = None }
                        match (last_ok, stack) {
                            (Some(last_ok), Some(stack)) => {
                                *last_ok = Some(x.clone());
                                stack.push(x);
                            }
                            (Some(last_ok), None) => *last_ok = Some(x),
                            (None, Some(stack)) => stack.push(x),
                            (None, None) => (),
                        }
                    }
                    None => assert!(last_ok.is_none() && stack.is_none()),
                }
                Err(x) => match error_scheme {
                    ErrorScheme::Soft => {
                        let x = Value::String(Rc::new(x));

                        if let Some(last_ok) = last_ok { *last_ok = None }
                        match (last_err, stack) {
                            (Some(last_err), Some(stack)) => {
                                *last_err = Some(x.clone());
                                stack.push(x);
                            }
                            (Some(last_err), None) => *last_err = Some(x),
                            (None, Some(stack)) => stack.push(x),
                            (None, None) => (),
                        }
                    }
                    ErrorScheme::Hard => return Err(ErrorCause::Promoted { error: x }),
                }
            }
            Ok(())
        }

        macro_rules! process_command {
            ($res:ident, $aft_pos:expr) => {{
                process_result($res, ErrorScheme::Hard, None, None, None, |_: ()| None)?;
                self.pos = $aft_pos;
            }}
        }
        macro_rules! process_request {
            ($res:ident, $action:expr, $aft_pos:expr) => {{
                match $action {
                    RequestAction::Syscall => process_result($res, global_context.settings.syscall_error_scheme, Some(&mut self.value_stack), None, Some(&mut self.last_syscall_error), Some)?,
                    RequestAction::Rpc => process_result($res, global_context.settings.rpc_error_scheme, Some(&mut self.value_stack), None, Some(&mut self.last_rpc_error), Some)?,
                    RequestAction::Input => process_result($res, ErrorScheme::Hard, None, Some(&mut self.last_answer), None, Some)?,
                    RequestAction::Push => process_result($res, ErrorScheme::Hard, Some(&mut self.value_stack), None, None, Some)?,
                }
                self.pos = $aft_pos;
            }}
        }

        match &self.defer {
            None => (),
            Some(Defer::Request { key, aft_pos, action }) => match global_context.system.poll_request(mc, key, &*self.entity.read())? {
                AsyncResult::Completed(x) => {
                    process_request!(x, action, *aft_pos);
                    self.defer = None;
                }
                AsyncResult::Pending => return Ok(ProcessStep::Yield),
                AsyncResult::Consumed => panic!(),
            }
            Some(Defer::Command { key, aft_pos }) => match global_context.system.poll_command(mc, key, &*self.entity.read())? {
                AsyncResult::Completed(x) => {
                    process_command!(x, *aft_pos);
                    self.defer = None;
                }
                AsyncResult::Pending => return Ok(ProcessStep::Yield),
                AsyncResult::Consumed => panic!(),
            }
            Some(Defer::MessageReply { key, aft_pos }) => match global_context.system.poll_reply(key) {
                AsyncResult::Completed(x) => {
                    let value = match x {
                        Some(x) => Value::from_json(mc, x)?,
                        None => empty_string().into(),
                    };
                    self.value_stack.push(value);
                    self.pos = *aft_pos;
                    self.defer = None;
                }
                AsyncResult::Pending => return Ok(ProcessStep::Yield),
                AsyncResult::Consumed => panic!(),
            }
            Some(Defer::Barrier { condition, aft_pos }) => match condition.is_completed() {
                true => {
                    self.pos = *aft_pos;
                    self.defer = None;
                }
                false => return Ok(ProcessStep::Yield),
            }
            Some(Defer::Sleep { until, aft_pos }) => match global_context.system.time_ms()? >= *until {
                true => {
                    self.pos = *aft_pos;
                    self.defer = None;
                }
                false => return Ok(ProcessStep::Yield),
            }
        }

        let mut entity = self.entity.write(mc);
        let mut context = [&mut global_context.globals, &mut entity.fields, &mut self.call_stack.last_mut().unwrap().locals];
        let mut context = LookupGroup::new(&mut context);

        macro_rules! lookup_var {
            ($var:expr => $m:ident) => {{
                let var = $var;
                match context.$m(var) {
                    Some(x) => x,
                    None => return Err(ErrorCause::UndefinedVariable { name: var.into() }),
                }
            }};
            ($var:expr) => {lookup_var!($var => lookup)};
            (mut $var:expr) => {lookup_var!($var => lookup_mut)};
        }

        macro_rules! perform_command {
            ($command:expr, $aft_pos:expr) => {{
                match global_context.system.perform_command(mc, $command, &*entity)? {
                    MaybeAsync::Async(key) => self.defer = Some(Defer::Command { key, aft_pos: $aft_pos }),
                    MaybeAsync::Sync(res) => process_command!(res, $aft_pos),
                }
            }}
        }
        macro_rules! perform_request {
            ($request:expr, $action:expr, $aft_pos:expr) => {{
                match global_context.system.perform_request(mc, $request, &*entity)? {
                    MaybeAsync::Async(key) => self.defer = Some(Defer::Request { key, aft_pos: $aft_pos, action: $action }),
                    MaybeAsync::Sync(res) => process_request!(res, $action, $aft_pos),
                }
            }}
        }

        let (ins, aft_pos) = Instruction::read(&global_context.bytecode.code, &global_context.bytecode.data, self.pos);
        match ins {
            Instruction::Yield => {
                self.pos = aft_pos;
                if self.warp_counter == 0 { return Ok(ProcessStep::Yield) }
            }
            Instruction::WarpStart => {
                self.warp_counter += 1;
                self.pos = aft_pos;
            }
            Instruction::WarpStop => {
                self.warp_counter -= 1;
                self.pos = aft_pos;
            }

            Instruction::PushBool { value } => {
                self.value_stack.push(value.into());
                self.pos = aft_pos;
            }
            Instruction::PushInt { value } => {
                self.value_stack.push(Number::new(value as f64)?.into());
                self.pos = aft_pos;
            }
            Instruction::PushNumber { value } => {
                self.value_stack.push(Number::new(value)?.into());
                self.pos = aft_pos;
            }
            Instruction::PushString { value } => {
                self.value_stack.push(Value::String(Rc::new(value.to_owned())));
                self.pos = aft_pos;
            }
            Instruction::PushVariable { var } => {
                self.value_stack.push(lookup_var!(var).get().clone());
                self.pos = aft_pos;
            }
            Instruction::PopValue => {
                self.value_stack.pop().unwrap();
                self.pos = aft_pos;
            }

            Instruction::DupeValue { top_index } => {
                let val = self.value_stack[self.value_stack.len() - 1 - top_index as usize].clone();
                self.value_stack.push(val);
                self.pos = aft_pos;
            }
            Instruction::SwapValues { top_index_1, top_index_2 } => {
                let len = self.value_stack.len();
                self.value_stack.swap(len - 1 - top_index_1 as usize, len - 1 - top_index_2 as usize);
                self.pos = aft_pos;
            }

            Instruction::ToBool => {
                let val = self.value_stack.pop().unwrap();
                self.value_stack.push(val.to_bool()?.into());
                self.pos = aft_pos;
            }
            Instruction::ToNumber => {
                let val = self.value_stack.pop().unwrap();
                self.value_stack.push(val.to_number()?.into());
                self.pos = aft_pos;
            }

            Instruction::ListCons => {
                let mut res = self.value_stack.pop().unwrap().as_list()?.read().clone();
                res.push_front(self.value_stack.pop().unwrap());
                self.value_stack.push(GcCell::allocate(mc, res).into());
                self.pos = aft_pos;
            }
            Instruction::ListCdr => {
                let mut res = self.value_stack.pop().unwrap().as_list()?.read().clone();
                if res.is_empty() { return Err(ErrorCause::IndexOutOfBounds { index: 1.0, len: 0 }) }
                res.pop_front().unwrap();
                self.value_stack.push(GcCell::allocate(mc, res).into());
                self.pos = aft_pos;
            }

            Instruction::ListFind => {
                let list = self.value_stack.pop().unwrap().as_list()?;
                let value = self.value_stack.pop().unwrap();
                let res = list.read().iter().enumerate().find(|(_, x)| ops::check_eq(x, &value)).map(|(i, _)| i + 1).unwrap_or(0);
                self.value_stack.push(Number::new(res as f64)?.into());
                self.pos = aft_pos;
            }
            Instruction::ListContains => {
                let value = self.value_stack.pop().unwrap();
                let res = self.value_stack.pop().unwrap().as_list()?.read().iter().any(|x| ops::check_eq(x, &value));
                self.value_stack.push(res.into());
                self.pos = aft_pos;
            }

            Instruction::ListIsEmpty => {
                let list = self.value_stack.pop().unwrap().as_list()?;
                self.value_stack.push(list.read().is_empty().into());
                self.pos = aft_pos;
            }
            Instruction::ListLength => {
                let list = self.value_stack.pop().unwrap().as_list()?;
                self.value_stack.push(Number::new(list.read().len() as f64)?.into());
                self.pos = aft_pos;
            }
            Instruction::ListDims => {
                let list = self.value_stack.pop().unwrap();
                self.value_stack.push(GcCell::allocate(mc, ops::dimensions(&list)?.into_iter().map(|x| Ok(Number::new(x as f64)?.into())).collect::<Result<VecDeque<_>, NumberError>>()?).into());
                self.pos = aft_pos;
            }
            Instruction::ListRank => {
                let list = self.value_stack.pop().unwrap();
                self.value_stack.push(Number::new(ops::dimensions(&list)?.len() as f64)?.into());
                self.pos = aft_pos;
            }

            Instruction::ListRev => {
                let list = self.value_stack.pop().unwrap().as_list()?;
                self.value_stack.push(GcCell::allocate(mc, list.read().iter().rev().cloned().collect::<VecDeque<_>>()).into());
                self.pos = aft_pos;
            }
            Instruction::ListFlatten => {
                let list = self.value_stack.pop().unwrap();
                self.value_stack.push(GcCell::allocate(mc, ops::flatten(&list)?).into());
                self.pos = aft_pos;
            }
            Instruction::ListReshape { len } => {
                let raw_dims: Vec<_> = match len {
                    VariadicLen::Fixed(len) => {
                        let stack_size = self.value_stack.len();
                        self.value_stack.drain(stack_size - len..).collect()
                    }
                    VariadicLen::Dynamic => self.value_stack.pop().unwrap().as_list()?.read().iter().cloned().collect(),
                };
                let src = self.value_stack.pop().unwrap();

                let mut dims = Vec::with_capacity(raw_dims.len());
                for dim in raw_dims {
                    let dim = dim.to_number()?.get();
                    if dim < 0.0 || dim > usize::MAX as f64 { return Err(ErrorCause::InvalidSize { value: dim }) }
                    let int_dim = dim as usize;
                    if int_dim as f64 != dim { return Err(ErrorCause::InvalidSize { value: dim }) }
                    dims.push(int_dim);
                }

                self.value_stack.push(ops::reshape(mc, &src, &dims)?);
                self.pos = aft_pos;
            }
            Instruction::ListCartesianProduct { len } => {
                let sources: Vec<_> = match len {
                    VariadicLen::Fixed(len) => {
                        let stack_size = self.value_stack.len();
                        self.value_stack.drain(stack_size - len..).map(|x| x.as_list()).collect::<Result<_,_>>()?
                    }
                    VariadicLen::Dynamic => self.value_stack.pop().unwrap().as_list()?.read().iter().map(|x| x.as_list()).collect::<Result<_,_>>()?,
                };
                self.value_stack.push(GcCell::allocate(mc, ops::cartesian_product(mc, &sources)).into());
                self.pos = aft_pos;
            }

            Instruction::ListJson => {
                let value = self.value_stack.pop().unwrap().to_json()?;
                self.value_stack.push(Rc::new(value.to_string()).into());
                self.pos = aft_pos;
            }

            Instruction::ListInsert => {
                let list = self.value_stack.pop().unwrap().as_list()?;
                let index = self.value_stack.pop().unwrap();
                let val = self.value_stack.pop().unwrap();
                let mut list = list.write(mc);

                let index = ops::prep_index(&index, list.len() + 1)?;
                list.insert(index, val);
                self.pos = aft_pos;
            }
            Instruction::ListInsertLast => {
                let list = self.value_stack.pop().unwrap().as_list()?;
                let val = self.value_stack.pop().unwrap();
                list.write(mc).push_back(val);
                self.pos = aft_pos;
            }
            Instruction::ListInsertRandom => {
                let list = self.value_stack.pop().unwrap().as_list()?;
                let val = self.value_stack.pop().unwrap();
                let mut list = list.write(mc);

                let index = ops::prep_rand_index(&*global_context.system, list.len() + 1)?;
                list.insert(index, val);
                self.pos = aft_pos;
            }

            Instruction::ListGet => {
                let list = self.value_stack.pop().unwrap();
                let index = self.value_stack.pop().unwrap();
                self.value_stack.push(ops::index_list(mc, &*global_context.system, &list, &index)?);
                self.pos = aft_pos;
            }
            Instruction::ListGetLast => {
                let list = self.value_stack.pop().unwrap().as_list()?;
                self.value_stack.push(match list.read().back() {
                    Some(x) => x.clone(),
                    None => return Err(ErrorCause::IndexOutOfBounds { index: 1.0, len: 0 }),
                });
                self.pos = aft_pos;
            }
            Instruction::ListGetRandom => {
                let list = self.value_stack.pop().unwrap().as_list()?;
                let list = list.read();
                let index = ops::prep_rand_index(&*global_context.system, list.len())?;
                self.value_stack.push(list[index].clone());
                self.pos = aft_pos;
            }

            Instruction::ListAssign => {
                let value = self.value_stack.pop().unwrap();
                let list = self.value_stack.pop().unwrap().as_list()?;
                let index = self.value_stack.pop().unwrap();
                let mut list = list.write(mc);

                let index = ops::prep_index(&index, list.len())?;
                list[index] = value;
                self.pos = aft_pos;
            }
            Instruction::ListAssignLast => {
                let value = self.value_stack.pop().unwrap();
                let list = self.value_stack.pop().unwrap().as_list()?;
                let mut list = list.write(mc);
                if list.is_empty() { return Err(ErrorCause::IndexOutOfBounds { index: 1.0, len: 0 }); }
                *list.back_mut().unwrap() = value;
                self.pos = aft_pos;
            }
            Instruction::ListAssignRandom => {
                let value = self.value_stack.pop().unwrap();
                let list = self.value_stack.pop().unwrap().as_list()?;
                let mut list = list.write(mc);

                let index = ops::prep_rand_index(&*global_context.system, list.len())?;
                list[index] = value;
                self.pos = aft_pos;
            }

            Instruction::ListRemove => {
                let list = self.value_stack.pop().unwrap().as_list()?;
                let index = self.value_stack.pop().unwrap();
                let mut list = list.write(mc);
                let index = ops::prep_index(&index, list.len())?;
                list.remove(index);
                self.pos = aft_pos;
            }
            Instruction::ListRemoveLast => {
                let list = self.value_stack.pop().unwrap().as_list()?;
                let mut list = list.write(mc);
                if list.is_empty() { return Err(ErrorCause::IndexOutOfBounds { index: 1.0, len: 0 }) }
                list.pop_back().unwrap();
                self.pos = aft_pos;
            }
            Instruction::ListRemoveAll => {
                self.value_stack.pop().unwrap().as_list()?.write(mc).clear();
                self.pos = aft_pos;
            }

            Instruction::ListPopFirstOrElse { goto } => match self.value_stack.pop().unwrap().as_list()?.write(mc).pop_front() {
                Some(value) => {
                    self.value_stack.push(value);
                    self.pos = aft_pos;
                }
                None => self.pos = goto,
            }

            Instruction::BinaryOp { op } => {
                let b = self.value_stack.pop().unwrap();
                let a = self.value_stack.pop().unwrap();
                self.value_stack.push(ops::binary_op(mc, &*global_context.system, &a, &b, op)?);
                self.pos = aft_pos;
            }
            Instruction::VariadicOp { op, len } => {
                fn combine_as_binary<'gc, S: System>(mc: MutationContext<'gc, '_>, system: &S, mut acc: Value<'gc, S>, values: &mut dyn Iterator<Item = &Value<'gc, S>>, op: BinaryOp) -> Result<Value<'gc, S>, ErrorCause<S>> {
                    for item in values {
                        acc = ops::binary_op(mc, system, &acc, item, op)?;
                    }
                    Ok(acc)
                }

                type Combine<'gc, S, I> = fn(MutationContext<'gc, '_>, &S, I) -> Result<Value<'gc, S>, ErrorCause<S>>;
                let combine: Combine<'gc, S, &mut dyn Iterator<Item = &Value<'gc, S>>> = match op {
                    VariadicOp::Add => |mc, system, values| combine_as_binary(mc, system, Value::Number(Number::new(0.0)?), values, BinaryOp::Add),
                    VariadicOp::Mul => |mc, system, values| combine_as_binary(mc, system, Value::Number(Number::new(1.0)?), values, BinaryOp::Mul),
                    VariadicOp::Min => |mc, system, values| combine_as_binary(mc, system, Value::Number(Number::infinity()?), values, BinaryOp::Min),
                    VariadicOp::Max => |mc, system, values| combine_as_binary(mc, system, Value::Number(Number::neg_infinity()?), values, BinaryOp::Max),
                    VariadicOp::StrCat => |_, _, values| {
                        let mut acc = String::new();
                        for item in values {
                            acc.push_str(item.to_string()?.as_ref());
                        }
                        Ok(Rc::new(acc).into())
                    },
                    VariadicOp::MakeList => |mc, _, values| {
                        Ok(GcCell::allocate(mc, values.cloned().collect::<VecDeque<_>>()).into())
                    },
                    VariadicOp::ListCat => |mc, _, values| {
                        let mut acc = VecDeque::new();
                        for item in values {
                            acc.extend(item.as_list()?.read().iter().cloned());
                        }
                        Ok(GcCell::allocate(mc, acc).into())
                    },
                };

                let res = match len {
                    VariadicLen::Fixed(len) => {
                        let stack_size = self.value_stack.len();
                        let res = combine(mc, &*global_context.system, &mut self.value_stack[stack_size - len..].iter())?;
                        self.value_stack.drain(stack_size - len..);
                        res
                    }
                    VariadicLen::Dynamic => {
                        let src = self.value_stack.pop().unwrap().as_list()?;
                        let src = src.read();
                        combine(mc, &*global_context.system, &mut src.iter())?
                    }
                };
                self.value_stack.push(res);
                self.pos = aft_pos;
            }
            Instruction::Eq { negate } => {
                let b = self.value_stack.pop().unwrap();
                let a = self.value_stack.pop().unwrap();
                self.value_stack.push((ops::check_eq(&a, &b) ^ negate).into());
                self.pos = aft_pos;
            }
            Instruction::RefEq => {
                let b = self.value_stack.pop().unwrap();
                let a = self.value_stack.pop().unwrap();
                self.value_stack.push(ops::check_ref_eq(&a, &b).into());
                self.pos = aft_pos;
            }
            Instruction::UnaryOp { op } => {
                let x = self.value_stack.pop().unwrap();
                self.value_stack.push(ops::unary_op(mc, &*global_context.system, &x, op)?);
                self.pos = aft_pos;
            }

            Instruction::DeclareLocal { var } => {
                context.locals_mut().redefine_or_define(var, Shared::Unique(Number::new(0.0)?.into()));
                self.pos = aft_pos;
            }
            Instruction::Assign { var } => {
                let value = self.value_stack.pop().unwrap();
                context.set_or_define(mc, var, value);
                self.pos = aft_pos;
            }
            Instruction::BinaryOpAssign { var, op } => {
                let b = self.value_stack.pop().unwrap();
                let a = lookup_var!(var).get().clone();
                context.set_or_define(mc, var, ops::binary_op(mc, &*global_context.system, &a, &b, op)?);
                self.pos = aft_pos;
            }

            Instruction::Jump { to } => self.pos = to,
            Instruction::ConditionalJump { to, when } => {
                let value = self.value_stack.pop().unwrap();
                self.pos = if value.to_bool()? == when { to } else { aft_pos };
            }

            Instruction::MetaPush { value } => {
                self.meta_stack.push(value.to_owned());
                self.pos = aft_pos;
            }

            Instruction::Call { pos, params } => {
                if self.call_stack.len() >= global_context.settings.max_call_depth {
                    return Err(ErrorCause::CallDepthLimit { limit: global_context.settings.max_call_depth });
                }

                debug_assert_eq!(self.meta_stack.len(), params);
                let params: Vec<_> = self.meta_stack.drain(..).collect();

                let mut locals = SymbolTable::default();
                for var in params.iter().rev() {
                    locals.redefine_or_define(var, self.value_stack.pop().unwrap().into());
                }
                self.call_stack.push(CallStackEntry {
                    called_from: self.pos,
                    return_to: aft_pos,
                    warp_counter: self.warp_counter,
                    value_stack_size: self.value_stack.len(),
                    handler_stack_size: self.handler_stack.len(),
                    locals
                });
                self.pos = pos;
            }
            Instruction::MakeClosure { pos, params, captures } => {
                debug_assert_eq!(self.meta_stack.len(), params + captures);
                let captures: Vec<_> = self.meta_stack.drain(params..).collect();
                let params: Vec<_> = self.meta_stack.drain(..).collect();

                let mut caps = SymbolTable::default();
                for var in captures.iter() {
                    caps.redefine_or_define(var, lookup_var!(mut var).alias(mc));
                }
                self.value_stack.push(GcCell::allocate(mc, Closure { pos, params, captures: caps }).into());
                self.pos = aft_pos;
            }
            Instruction::CallClosure { args } => {
                let closure = self.value_stack.pop().unwrap().as_closure()?;
                let mut closure = closure.write(mc);
                if closure.params.len() != args {
                    return Err(ErrorCause::ClosureArgCount { expected: closure.params.len(), got: args });
                }

                let mut locals = SymbolTable::default();
                for (k, v) in closure.captures.iter_mut() {
                    locals.redefine_or_define(k, v.alias(mc));
                }
                for var in closure.params.iter().rev() {
                    locals.redefine_or_define(var, self.value_stack.pop().unwrap().into());
                }
                self.call_stack.push(CallStackEntry {
                    called_from: self.pos,
                    return_to: aft_pos,
                    warp_counter: self.warp_counter,
                    value_stack_size: self.value_stack.len(),
                    handler_stack_size: self.handler_stack.len(),
                    locals,
                });
                self.pos = closure.pos;
            }
            Instruction::Return => {
                let CallStackEntry { called_from, return_to, locals: _, warp_counter, value_stack_size, handler_stack_size } = self.call_stack.pop().unwrap();
                let return_value = self.value_stack.pop().unwrap();

                self.pos = return_to;
                self.warp_counter = warp_counter;
                self.value_stack.drain(value_stack_size..);
                self.handler_stack.drain(handler_stack_size..);
                debug_assert_eq!(self.value_stack.len(), value_stack_size);
                debug_assert_eq!(self.handler_stack.len(), handler_stack_size);

                self.value_stack.push(return_value);

                if self.call_stack.is_empty() {
                    debug_assert_eq!(self.value_stack.len(), 1);
                    debug_assert_eq!(called_from, usize::MAX);
                    debug_assert_eq!(return_to, usize::MAX);
                    debug_assert_eq!(warp_counter, 0);
                    debug_assert_eq!(value_stack_size, 0);
                    debug_assert_eq!(handler_stack_size, 0);
                    return Ok(ProcessStep::Terminate { result: Some(self.value_stack.pop().unwrap()) });
                }
            }
            Instruction::PushHandler { pos, var } => {
                self.handler_stack.push(Handler {
                    pos,
                    var: var.to_owned(),
                    warp_counter: self.warp_counter,
                    call_stack_size: self.call_stack.len(),
                    value_stack_size: self.value_stack.len(),
                });
                self.pos = aft_pos;
            }
            Instruction::PopHandler => {
                self.handler_stack.pop().unwrap();
                self.pos = aft_pos;
            }
            Instruction::Throw => {
                let msg = self.value_stack.pop().unwrap().to_string()?.into_owned();
                return Err(ErrorCause::Custom { msg });
            }
            Instruction::CallRpc { service, rpc, args } => {
                debug_assert_eq!(self.meta_stack.len(), args);
                let mut args_vec = Vec::with_capacity(args);
                for _ in 0..args {
                    let arg_name = self.meta_stack.pop().unwrap();
                    let value = self.value_stack.pop().unwrap();
                    args_vec.push((arg_name, value));
                }
                args_vec.reverse();
                perform_request!(Request::Rpc { service: service.to_owned(), rpc: rpc.to_owned(), args: args_vec}, RequestAction::Rpc, aft_pos);
            }
            Instruction::PushRpcError => {
                self.value_stack.push(self.last_rpc_error.clone().unwrap_or_else(|| empty_string().into()));
                self.pos = aft_pos;
            }
            Instruction::Syscall { len } => {
                let args = match len {
                    VariadicLen::Fixed(len) => {
                        let stack_size = self.value_stack.len();
                        self.value_stack.drain(stack_size - len..).collect()
                    }
                    VariadicLen::Dynamic => self.value_stack.pop().unwrap().as_list()?.read().iter().cloned().collect(),
                };
                let name = self.value_stack.pop().unwrap().to_string()?.into_owned();
                perform_request!(Request::Syscall { name, args }, RequestAction::Syscall, aft_pos);
            }
            Instruction::PushSyscallError => {
                self.value_stack.push(self.last_syscall_error.clone().unwrap_or_else(|| empty_string().into()));
                self.pos = aft_pos;
            }
            Instruction::Broadcast { wait } => {
                let msg_type = self.value_stack.pop().unwrap().to_string()?.into_owned();
                let barrier = match wait {
                    false => {
                        self.pos = aft_pos;
                        None
                    }
                    true => {
                        let barrier = Barrier::new();
                        self.defer = Some(Defer::Barrier { condition: barrier.get_condition(), aft_pos });
                        Some(barrier)
                    }
                };
                return Ok(ProcessStep::Broadcast { msg_type, barrier });
            }
            Instruction::Print => {
                let value = self.value_stack.pop().unwrap();
                let is_empty = match &value { Value::String(x) => x.is_empty(), _ => false };
                perform_command!(Command::Print { value: if is_empty { None } else { Some(value) } }, aft_pos);
            }
            Instruction::Ask => {
                let prompt = self.value_stack.pop().unwrap();
                let is_empty = match &prompt { Value::String(x) => x.is_empty(), _ => false };
                perform_request!(Request::Input { prompt: if is_empty { None } else { Some(prompt) } }, RequestAction::Input, aft_pos);
            }
            Instruction::PushAnswer => {
                self.value_stack.push(self.last_answer.clone().unwrap_or_else(|| empty_string().into()));
                self.pos = aft_pos;
            }
            Instruction::ResetTimer => {
                global_context.timer_start = global_context.system.time_ms()?;
                self.pos = aft_pos;
            }
            Instruction::PushTimer => {
                self.value_stack.push(Number::new(global_context.system.time_ms()?.saturating_sub(global_context.timer_start) as f64 / 1000.0)?.into());
                self.pos = aft_pos;
            }
            Instruction::Sleep => {
                let ms = self.value_stack.pop().unwrap().to_number()?.get() * 1000.0;
                if ms <= 0.0 {
                    self.pos = aft_pos;
                    return Ok(ProcessStep::Yield);
                }
                self.defer = Some(Defer::Sleep { until: global_context.system.time_ms()? + ms as u64, aft_pos });
            }
            Instruction::SendNetworkMessage { msg_type, values, expect_reply } => {
                let targets = match self.value_stack.pop().unwrap() {
                    Value::String(x) => vec![x.as_str().to_owned()],
                    Value::List(x) => {
                        let x = x.read();
                        let mut res = Vec::with_capacity(x.len());
                        for val in x.iter() {
                            match val {
                                Value::String(x) => res.push(x.as_str().to_owned()),
                                x => return Err(ErrorCause::VariadicConversionError { got: x.get_type(), expected: Type::String }),
                            }
                        }
                        res
                    }
                    x => return Err(ErrorCause::VariadicConversionError { got: x.get_type(), expected: Type::String }),
                };
                let values = {
                    let mut res = Vec::with_capacity(values);
                    for _ in 0..values {
                        let value = self.value_stack.pop().unwrap().to_json()?;
                        let field = self.meta_stack.pop().unwrap();
                        res.push((field, value));
                    }
                    res
                };
                match global_context.system.send_message(msg_type.into(), values, targets, expect_reply)? {
                    Some(key) => self.defer = Some(Defer::MessageReply { key, aft_pos }),
                    None => self.pos = aft_pos,
                }
            }
            Instruction::SendNetworkReply => {
                let value = self.value_stack.pop().unwrap().to_json()?;
                if let Some(key) = self.reply_key.take() {
                    global_context.system.send_reply(key, value)?;
                }
                self.pos = aft_pos;
            }
            Instruction::PushPosition => perform_request!(Request::Position, RequestAction::Push, aft_pos),
            Instruction::PushHeading => perform_request!(Request::Heading, RequestAction::Push, aft_pos),
            Instruction::Forward => {
                let distance = self.value_stack.pop().unwrap().to_number()?;
                perform_command!(Command::Forward { distance }, aft_pos);
            }
            Instruction::Turn { right } => {
                let mut angle = self.value_stack.pop().unwrap().to_number()?;
                if !right { angle = angle.neg()? }
                perform_command!(Command::Turn { angle }, aft_pos);
            }
            Instruction::PushEffect { kind } => {
                self.value_stack.push(entity.effects.get_effect_mut(kind).clone().into());
                self.pos = aft_pos;
            }
            Instruction::SetEffect { kind } => {
                let value = self.value_stack.pop().unwrap().to_number()?;
                *entity.effects.get_effect_mut(kind) = value;
                self.pos = aft_pos;
            }
            Instruction::ChangeEffect { kind } => {
                let delta = self.value_stack.pop().unwrap().to_number()?;
                let effect = entity.effects.get_effect_mut(kind);
                *effect = effect.add(delta)?;
                self.pos = aft_pos;
            }
        }

        Ok(ProcessStep::Normal)
    }
}

mod ops {
    use super::*;

    fn as_list<'gc, S: System>(v: &Value<'gc, S>) -> Option<GcCell<'gc, VecDeque<Value<'gc, S>>>> {
        match v {
            Value::List(v) => Some(*v),
            _ => None
        }
    }
    fn as_matrix<'gc, S: System>(v: &Value<'gc, S>) -> Option<GcCell<'gc, VecDeque<Value<'gc, S>>>> {
        let vals = as_list(v)?;
        let good = match vals.read().front() {
            None => false,
            Some(first) => as_list(first).is_some(),
        };
        if good { Some(vals) } else { None }
    }

    pub(super) fn prep_index<S: System>(index: &Value<'_, S>, len: usize) -> Result<usize, ErrorCause<S>> {
        let raw_index = index.to_number()?.get();
        if raw_index < 1.0 || raw_index > len as f64 { return Err(ErrorCause::IndexOutOfBounds { index: raw_index, len }) }
        let index = raw_index as u64;
        if index as f64 != raw_index { return Err(ErrorCause::IndexNotInteger { index: raw_index }) }
        Ok(index as usize - 1)
    }
    pub(super) fn prep_rand_index<S: System>(system: &S, len: usize) -> Result<usize, ErrorCause<S>> {
        if len == 0 { return Err(ErrorCause::IndexOutOfBounds { index: 1.0, len: 0 }) }
        system.rand(0..len)
    }

    pub(super) fn flatten<'gc, S: System>(value: &Value<'gc, S>) -> Result<VecDeque<Value<'gc, S>>, ErrorCause<S>> {
        fn flatten_impl<'gc, S: System>(value: &Value<'gc, S>, dest: &mut VecDeque<Value<'gc, S>>, cache: &mut BTreeSet<Identity<'gc, S>>) -> Result<(), ErrorCause<S>> {
            match value {
                Value::List(values) => {
                    let key = value.identity();
                    if !cache.insert(key) { return Err(ErrorCause::CyclicValue) }
                    for value in values.read().iter() {
                        flatten_impl(value, dest, cache)?;
                    }
                    cache.remove(&key);
                }
                _ => dest.push_back(value.clone()),
            }
            Ok(())
        }
        let mut res = Default::default();
        let mut cache = Default::default();
        flatten_impl(value, &mut res, &mut cache)?;
        debug_assert_eq!(cache.len(), 0);
        Ok(res)
    }
    pub(super) fn dimensions<S: System>(value: &Value<'_, S>) -> Result<Vec<usize>, ErrorCause<S>> {
        fn dimensions_impl<'gc, S: System>(value: &Value<'gc, S>, depth: usize, res: &mut Vec<usize>, cache: &mut BTreeSet<Identity<'gc, S>>) -> Result<(), ErrorCause<S>> {
            debug_assert!(depth <= res.len());

            if let Value::List(values) = value {
                if depth == res.len() { res.push(0); }

                let key = value.identity();
                if !cache.insert(key) { return Err(ErrorCause::CyclicValue) }

                let values = values.read();
                res[depth] = res[depth].max(values.len());
                for value in values.iter() {
                    dimensions_impl(value, depth + 1, res, cache)?;
                }

                cache.remove(&key);
            }
            Ok(())
        }
        let mut res = Default::default();
        let mut cache = Default::default();
        dimensions_impl(value, 0, &mut res, &mut cache)?;
        debug_assert_eq!(cache.len(), 0);
        Ok(res)
    }
    pub(super) fn reshape<'gc, S: System>(mc: MutationContext<'gc, '_>, src: &Value<'gc, S>, dims: &[usize]) -> Result<Value<'gc, S>, ErrorCause<S>> {
        if dims.iter().any(|&x| x == 0) {
            return Ok(GcCell::allocate(mc, VecDeque::default()).into())
        }

        let mut src = ops::flatten(src)?;
        if src.is_empty() {
            src.push_back(empty_string().into());
        }

        fn reshape_impl<'gc, S: System>(mc: MutationContext<'gc, '_>, src: &mut Cycle<VecDequeIter<Value<'gc, S>>>, dims: &[usize]) -> Value<'gc, S> {
            match dims {
                [] => src.next().unwrap().clone(),
                [first, rest @ ..] => GcCell::allocate(mc, (0..*first).map(|_| reshape_impl(mc, src, rest)).collect::<VecDeque<_>>()).into(),
            }
        }
        Ok(reshape_impl(mc, &mut src.iter().cycle(), dims))
    }
    pub(super) fn cartesian_product<'gc, S: System>(mc: MutationContext<'gc, '_>, sources: &[GcCell<VecDeque<Value<'gc, S>>>]) -> VecDeque<Value<'gc, S>> {
        if sources.is_empty() { return Default::default() }

        fn cartesian_product_impl<'gc, S: System>(mc: MutationContext<'gc, '_>, res: &mut VecDeque<Value<'gc, S>>, partial: &mut VecDeque<Value<'gc, S>>, sources: &[GcCell<VecDeque<Value<'gc, S>>>]) {
            match sources {
                [] => res.push_back(GcCell::allocate(mc, partial.clone()).into()),
                [first, rest @ ..] => for item in first.read().iter() {
                    partial.push_back(item.clone());
                    cartesian_product_impl(mc, res, partial, rest);
                    partial.pop_back();
                }
            }
        }
        let mut res = VecDeque::with_capacity(sources.iter().fold(1, |a, b| a * b.read().len()));
        let mut partial = VecDeque::with_capacity(sources.len());
        cartesian_product_impl(mc, &mut res, &mut partial, sources);
        res
    }

    fn cmp_values<'gc, S: System>(a: &Value<'gc, S>, b: &Value<'gc, S>) -> Result<Ordering, ErrorCause<S>> {
        Ok(match (a.to_number(), b.to_number()) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            _ => a.to_string()?.as_ref().cmp(b.to_string()?.as_ref()),
        })
    }

    fn binary_op_impl<'gc, S: System>(mc: MutationContext<'gc, '_>, system: &S, a: &Value<'gc, S>, b: &Value<'gc, S>, matrix_mode: bool, cache: &mut BTreeMap<(Identity<'gc, S>, Identity<'gc, S>, bool), Value<'gc, S>>, scalar_op: fn(MutationContext<'gc, '_>, &S, &Value<'gc, S>, &Value<'gc, S>) -> Result<Value<'gc, S>, ErrorCause<S>>) -> Result<Value<'gc, S>, ErrorCause<S>> {
        let cache_key = (a.identity(), b.identity(), matrix_mode);
        Ok(match cache.get(&cache_key) {
            Some(x) => x.clone(),
            None => {
                let checker = if matrix_mode { as_matrix } else { as_list };
                match (checker(a), checker(b)) {
                    (Some(a), Some(b)) => {
                        let (a, b) = (a.read(), b.read());
                        let real_res: Value<S> = GcCell::allocate(mc, VecDeque::with_capacity(a.len().min(b.len()))).into();
                        cache.insert(cache_key, real_res.clone());
                        let res = as_list(&real_res).unwrap();
                        let mut res = res.write(mc);
                        for (a, b) in iter::zip(&*a, &*b) {
                            res.push_back(binary_op_impl(mc, system, a, b, matrix_mode, cache, scalar_op)?);
                        }
                        real_res
                    }
                    (Some(a), None) => {
                        let a = a.read();
                        let real_res: Value<S> = GcCell::allocate(mc, VecDeque::with_capacity(a.len())).into();
                        cache.insert(cache_key, real_res.clone());
                        let res = as_list(&real_res).unwrap();
                        let mut res = res.write(mc);
                        for a in &*a {
                            res.push_back(binary_op_impl(mc, system, a, b, matrix_mode, cache, scalar_op)?);
                        }
                        real_res
                    }
                    (None, Some(b)) => {
                        let b = b.read();
                        let real_res: Value<S> = GcCell::allocate(mc, VecDeque::with_capacity(b.len())).into();
                        cache.insert(cache_key, real_res.clone());
                        let res = as_list(&real_res).unwrap();
                        let mut res = res.write(mc);
                        for b in &*b {
                            res.push_back(binary_op_impl(mc, system, a, b, matrix_mode, cache, scalar_op)?);
                        }
                        real_res
                    }
                    (None, None) => if matrix_mode { binary_op_impl(mc, system, a, b, false, cache, scalar_op)? } else { scalar_op(mc, system, a, b)? }
                }
            }
        })
    }
    pub(super) fn binary_op<'gc, 'a, S: System>(mc: MutationContext<'gc, '_>, system: &S, a: &'a Value<'gc, S>, b: &'a Value<'gc, S>, op: BinaryOp) -> Result<Value<'gc, S>, ErrorCause<S>> {
        let mut cache = Default::default();
        match op {
            BinaryOp::Add       => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| Ok(a.to_number()?.add(b.to_number()?)?.into())),
            BinaryOp::Sub       => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| Ok(a.to_number()?.sub(b.to_number()?)?.into())),
            BinaryOp::Mul       => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| Ok(a.to_number()?.mul(b.to_number()?)?.into())),
            BinaryOp::Div       => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| Ok(a.to_number()?.div(b.to_number()?)?.into())),
            BinaryOp::Pow       => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| Ok(a.to_number()?.powf(b.to_number()?)?.into())),
            BinaryOp::Log       => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| Ok(b.to_number()?.log(a.to_number()?)?.into())),
            BinaryOp::Atan2     => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| Ok(a.to_number()?.atan2(b.to_number()?)?.to_degrees()?.into())),
            BinaryOp::Greater   => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| Ok((cmp_values(a, b)? == Ordering::Greater).into())),
            BinaryOp::GreaterEq => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| Ok((cmp_values(a, b)? != Ordering::Less).into())),
            BinaryOp::Less      => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| Ok((cmp_values(a, b)? == Ordering::Less).into())),
            BinaryOp::LessEq    => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| Ok((cmp_values(a, b)? != Ordering::Greater).into())),
            BinaryOp::Min       => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| Ok(a.to_number()?.min(b.to_number()?).into())),
            BinaryOp::Max       => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| Ok(a.to_number()?.max(b.to_number()?).into())),

            BinaryOp::StrGet => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| {
                let string = b.to_string()?;
                let index = prep_index(a, string.chars().count())?;
                Ok(Rc::new(string.chars().nth(index).unwrap().to_string()).into())
            }),

            BinaryOp::Mod => binary_op_impl(mc, system, a, b, true, &mut cache, |_, _, a, b| {
                let (a, b) = (a.to_number()?.get(), b.to_number()?.get());
                Ok(Number::new(if a.is_sign_positive() == b.is_sign_positive() { a % b } else { b + (a % -b) })?.into())
            }),
            BinaryOp::SplitBy => binary_op_impl(mc, system, a, b, true, &mut cache, |mc, _, a, b| {
                let (text, pattern) = (a.to_string()?, b.to_string()?);
                Ok(GcCell::allocate(mc, text.split(pattern.as_ref()).map(|x| Rc::new(x.to_owned()).into()).collect::<VecDeque<_>>()).into())
            }),

            BinaryOp::Range => binary_op_impl(mc, system, a, b, true, &mut cache, |mc, _, a, b| {
                let (mut a, b) = (a.to_number()?.get(), b.to_number()?.get());
                let mut res = VecDeque::new();
                if a.is_finite() && b.is_finite() {
                    if a <= b {
                        while a <= b {
                            res.push_back(Number::new(a)?.into());
                            a += 1.0;
                        }
                    } else {
                        while a >= b {
                            res.push_back(Number::new(a)?.into());
                            a -= 1.0;
                        }
                    }
                }
                Ok(GcCell::allocate(mc, res).into())
            }),
            BinaryOp::Random => binary_op_impl(mc, system, a, b, true, &mut cache, |_, system, a, b| {
                let (mut a, mut b) = (a.to_number()?.get(), b.to_number()?.get());
                if a > b { (a, b) = (b, a); }
                let res = if a == libm::round(a) && b == libm::round(b) {
                    let (a, b) = (a as i64, b as i64);
                    system.rand(a..=b)? as f64
                } else {
                    system.rand(a..=b)?
                };
                Ok(Number::new(res)?.into())
            }),
        }
    }

    fn unary_op_impl<'gc, S: System>(mc: MutationContext<'gc, '_>, system: &S, x: &Value<'gc, S>, cache: &mut BTreeMap<Identity<'gc, S>, Value<'gc, S>>, scalar_op: &dyn Fn(MutationContext<'gc, '_>, &S, &Value<'gc, S>) -> Result<Value<'gc, S>, ErrorCause<S>>) -> Result<Value<'gc, S>, ErrorCause<S>> {
        let cache_key = x.identity();
        Ok(match cache.get(&cache_key) {
            Some(x) => x.clone(),
            None => match as_list(x) {
                Some(x) => {
                    let x = x.read();
                    let real_res: Value<S> = GcCell::allocate(mc, VecDeque::with_capacity(x.len())).into();
                    cache.insert(cache_key, real_res.clone());
                    let res = as_list(&real_res).unwrap();
                    let mut res = res.write(mc);
                    for x in &*x {
                        res.push_back(unary_op_impl(mc, system, x, cache, scalar_op)?);
                    }
                    real_res
                }
                None => scalar_op(mc, system, x)?,
            }
        })
    }
    pub(super) fn unary_op<'gc, S: System>(mc: MutationContext<'gc, '_>, system: &S, x: &Value<'gc, S>, op: UnaryOp) -> Result<Value<'gc, S>, ErrorCause<S>> {
        let mut cache = Default::default();
        match op {
            UnaryOp::Not    => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok((!x.to_bool()?).into())),
            UnaryOp::Abs    => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok(x.to_number()?.abs()?.into())),
            UnaryOp::Neg    => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok(x.to_number()?.neg()?.into())),
            UnaryOp::Sqrt   => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok(x.to_number()?.sqrt()?.into())),
            UnaryOp::Round  => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok(x.to_number()?.round()?.into())),
            UnaryOp::Floor  => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok(x.to_number()?.floor()?.into())),
            UnaryOp::Ceil   => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok(x.to_number()?.ceil()?.into())),
            UnaryOp::Sin    => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok(Number::new(libm::sin(x.to_number()?.get().to_radians()))?.into())),
            UnaryOp::Cos    => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok(Number::new(libm::cos(x.to_number()?.get().to_radians()))?.into())),
            UnaryOp::Tan    => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok(Number::new(libm::tan(x.to_number()?.get().to_radians()))?.into())),
            UnaryOp::Asin   => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok(Number::new(libm::asin(x.to_number()?.get()).to_degrees())?.into())),
            UnaryOp::Acos   => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok(Number::new(libm::acos(x.to_number()?.get()).to_degrees())?.into())),
            UnaryOp::Atan   => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok(Number::new(libm::atan(x.to_number()?.get()).to_degrees())?.into())),
            UnaryOp::StrLen => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| Ok(Number::new(x.to_string()?.chars().count() as f64)?.into())),

            UnaryOp::StrGetLast => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| match x.to_string()?.chars().rev().next() {
                Some(ch) => Ok(Rc::new(ch.to_string()).into()),
                None => return Err(ErrorCause::IndexOutOfBounds { index: 1.0, len: 0 }),
            }),
            UnaryOp::StrGetRandom => unary_op_impl(mc, system, x, &mut cache, &|_, system, x| {
                let x = x.to_string()?;
                let i = prep_rand_index(system, x.chars().count())?;
                Ok(Rc::new(x.chars().nth(i).unwrap().to_string()).into())
            }),

            UnaryOp::SplitLetter => unary_op_impl(mc, system, x, &mut cache, &|mc, _, x| {
                Ok(GcCell::allocate(mc, x.to_string()?.chars().map(|x| Rc::new(x.to_string()).into()).collect::<VecDeque<_>>()).into())
            }),
            UnaryOp::SplitWord => unary_op_impl(mc, system, x, &mut cache, &|mc, _, x| {
                Ok(GcCell::allocate(mc, x.to_string()?.split_whitespace().map(|x| Rc::new(x.to_owned()).into()).collect::<VecDeque<_>>()).into())
            }),
            UnaryOp::SplitTab => unary_op_impl(mc, system, x, &mut cache, &|mc, _, x| {
                Ok(GcCell::allocate(mc, x.to_string()?.split('\t').map(|x| Rc::new(x.to_owned()).into()).collect::<VecDeque<_>>()).into())
            }),
            UnaryOp::SplitCR => unary_op_impl(mc, system, x, &mut cache, &|mc, _, x| {
                Ok(GcCell::allocate(mc, x.to_string()?.split('\r').map(|x| Rc::new(x.to_owned()).into()).collect::<VecDeque<_>>()).into())
            }),
            UnaryOp::SplitLF => unary_op_impl(mc, system, x, &mut cache, &|mc, _, x| {
                Ok(GcCell::allocate(mc, x.to_string()?.lines().map(|x| Rc::new(x.to_owned()).into()).collect::<VecDeque<_>>()).into())
            }),
            UnaryOp::SplitCsv => unary_op_impl(mc, system, x, &mut cache, &|mc, _, x| {
                let lines = x.to_string()?.lines().map(|line| GcCell::allocate(mc, line.split(',').map(|x| Rc::new(x.to_owned()).into()).collect::<VecDeque<_>>()).into()).collect::<VecDeque<_>>();
                Ok(match lines.len() {
                    1 => lines.into_iter().next().unwrap(),
                    _ => GcCell::allocate(mc, lines).into(),
                })
            }),
            UnaryOp::SplitJson => unary_op_impl(mc, system, x, &mut cache, &|mc, _, x| {
                let value = x.to_string()?;
                match parse_json::<Json>(&*value) {
                    Ok(json) => Ok(Value::from_json(mc, json)?),
                    Err(_) => Err(ErrorCause::NotJson { value: value.into_owned() }),
                }
            }),

            UnaryOp::UnicodeToChar => unary_op_impl(mc, system, x, &mut cache, &|_, _, x| {
                let fnum = x.to_number()?.get();
                if fnum < 0.0 || fnum > u32::MAX as f64 { return Err(ErrorCause::InvalidUnicode { value: fnum }) }
                let num = fnum as u32;
                if num as f64 != fnum { return Err(ErrorCause::InvalidUnicode { value: fnum }) }
                match char::from_u32(num) {
                    Some(ch) => Ok(Rc::new(ch.to_string()).into()),
                    None => Err(ErrorCause::InvalidUnicode { value: fnum }),
                }
            }),
            UnaryOp::CharToUnicode => unary_op_impl(mc, system, x, &mut cache, &|mc, _, x| {
                let src = x.to_string()?;
                let values: VecDeque<_> = src.chars().map(|ch| Ok(Number::new(ch as u32 as f64)?.into())).collect::<Result<_, NumberError>>()?;
                Ok(match values.len() {
                    1 => values.into_iter().next().unwrap(),
                    _ => GcCell::allocate(mc, values).into(),
                })
            }),
        }
    }
    pub(super) fn index_list<'gc, S: System>(mc: MutationContext<'gc, '_>, system: &S, list: &Value<'gc, S>, index: &Value<'gc, S>) -> Result<Value<'gc, S>, ErrorCause<S>> {
        let list = list.as_list()?;
        let list = list.read();
        unary_op_impl(mc, system, index, &mut Default::default(), &|_, _, x| Ok(list[prep_index(x, list.len())?].clone()))
    }

    fn check_eq_impl<'gc, S: System>(a: &Value<'gc, S>, b: &Value<'gc, S>, cache: &mut BTreeSet<(Identity<'gc, S>, Identity<'gc, S>)>) -> bool {
        // if already cached, that cmp handles overall check, so no-op with true (if we ever get a false, the whole thing is false)
        if !cache.insert((a.identity(), b.identity())) { return true }

        match (a, b) {
            (Value::Bool(a), Value::Bool(b)) => *a == *b,
            (Value::Bool(_), _) | (_, Value::Bool(_)) => false,

            (Value::Number(a), Value::Number(b)) => *a == *b,
            (Value::String(a), Value::String(b)) => a.to_lowercase() == b.to_lowercase(),
            (Value::Number(n), Value::String(s)) | (Value::String(s), Value::Number(n)) => match s.parse::<f64>().ok().and_then(|x| Number::new(x).ok()) {
                Some(s) => s == *n,
                None => **s == n.to_string(),
            }
            (Value::Number(_), _) | (_, Value::Number(_)) => false,
            (Value::String(_), _) | (_, Value::String(_)) => false,

            (Value::Image(a), Value::Image(b)) => Rc::ptr_eq(a, b),
            (Value::Image(_), _) | (_, Value::Image(_)) => false,

            (Value::Closure(a), Value::Closure(b)) => a.as_ptr() == b.as_ptr(),
            (Value::Closure(_), _) | (_, Value::Closure(_)) => false,

            (Value::List(a), Value::List(b)) => {
                let (a, b) = (a.read(), b.read());
                if a.len() != b.len() { return false }
                for (a, b) in iter::zip(&*a, &*b) {
                    if !check_eq_impl(a, b, cache) { return false }
                }
                true
            }
            (Value::List(_), _) | (_, Value::List(_)) => false,

            (Value::Entity(a), Value::Entity(b)) => a.as_ptr() == b.as_ptr(),
            (Value::Entity(_), _) | (_, Value::Entity(_)) => false,

            (Value::Native(a), Value::Native(b)) => Rc::ptr_eq(a, b),
        }
    }
    pub(super) fn check_eq<'gc, S: System>(a: &Value<'gc, S>, b: &Value<'gc, S>) -> bool {
        check_eq_impl(a, b, &mut Default::default())
    }
    pub(super) fn check_ref_eq<'gc, S: System>(a: &Value<'gc, S>, b: &Value<'gc, S>) -> bool {
        match (a, b) {
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Bool(_), _) | (_, Value::Bool(_)) => false,

            (Value::Number(a), Value::Number(b)) => a == b,
            (Value::Number(_), _) | (_, Value::Number(_)) => false,

            (Value::String(a), Value::String(b)) => Rc::ptr_eq(a, b),
            (Value::String(_), _) | (_, Value::String(_)) => false,

            (Value::Image(a), Value::Image(b)) => Rc::ptr_eq(a, b),
            (Value::Image(_), _) | (_, Value::Image(_)) => false,

            (Value::Closure(a), Value::Closure(b)) => a.as_ptr() == b.as_ptr(),
            (Value::Closure(_), _) | (_, Value::Closure(_)) => false,

            (Value::List(a), Value::List(b)) => a.as_ptr() == b.as_ptr(),
            (Value::List(_), _) | (_, Value::List(_)) => false,

            (Value::Entity(a), Value::Entity(b)) => a.as_ptr() == b.as_ptr(),
            (Value::Entity(_), _) | (_, Value::Entity(_)) => false,

            (Value::Native(a), Value::Native(b)) => Rc::ptr_eq(a, b),
        }
    }
}
