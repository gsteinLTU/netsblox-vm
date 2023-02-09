use std::prelude::v1::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::marker::PhantomData;
use std::rc::{Rc, Weak};
use std::borrow::Cow;
use std::{iter, fmt};
use std::ops::Deref;
use std::cell::Ref;

use rand::distributions::uniform::{SampleUniform, SampleRange};

use crate::*;
use crate::gc::*;
use crate::json::*;
use crate::bytecode::*;

#[derive(Debug)]
pub enum FromAstError<'a> {
    BadNumber { error: NumberError },
    BadKeycode { key: String },
    UnsupportedEvent { kind: &'a ast::HatKind },
    CompileError { error: CompileError<'a> },
}
impl From<NumberError> for FromAstError<'_> { fn from(error: NumberError) -> Self { Self::BadNumber { error } } }
impl<'a> From<CompileError<'a>> for FromAstError<'a> { fn from(error: CompileError<'a>) -> Self { Self::CompileError { error } } }

#[derive(Debug)]
pub enum FromJsonError {
    HadNull,
    HadBadNumber,
}
#[derive(Educe)]
#[educe(Debug)]
pub enum ToJsonError<S: System> {
    /// The value was or contained a number which could not be expressed as [`f64`].
    BadNumber(f64),
    /// The value was or contained a type that cannot be exported as primitive [`Json`].
    ComplexType(Type<S>),
    /// The value contained a cycle, which [`Json`] forbids.
    Cyclic,
}

/// The type of a [`Value`].
#[derive(Educe)]
#[educe(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type<S: System> {
    Bool, Number, String, Image, List, Closure, Entity, Native(<S::NativeValue as GetType>::Output),
}

/// A type conversion error on a [`Value`].
#[derive(Educe)]
#[educe(Debug)]
pub struct ConversionError<S: System> {
    pub got: Type<S>,
    pub expected: Type<S>,
}

/// The cause/explanation of an execution error.
#[derive(Educe)]
#[educe(Debug)]
pub enum ErrorCause<S: System> {
    /// A variable lookup operation failed. `name` holds the name of the variable that was expected.
    UndefinedVariable { name: String },
    /// The result of a failed type conversion.
    ConversionError { got: Type<S>, expected: Type<S> },
    /// The result of a failed variadic type conversion (expected type `T` or a list of type `T`).
    VariadicConversionError { got: Type<S>, expected: Type<S> },
    /// An indexing operation on a list/string had an out of bounds index, `index`, on a list/string of size `len`. Note that Snap!/NetsBlox use 1-based indexing.
    IndexOutOfBounds { index: f64, len: usize },
    /// Attempt to index a list with a non-integer numeric value, `index`.
    IndexNotInteger { index: f64 },
    /// Attempt to use a number which was not a valid size (must be convertible to [`usize`]).
    InvalidSize { value: f64 },
    /// Attempt to interpret an invalid unicode code point (number) as a character.
    InvalidUnicode { value: f64 },
    /// Exceeded the maximum call depth.
    CallDepthLimit { limit: usize },
    /// Attempt to call a closure which required `expected` arguments, but `got` arguments were supplied.
    ClosureArgCount { expected: usize, got: usize },
    /// An acyclic operation received a cyclic input value.
    CyclicValue,
    /// Attempt to parse an invalid JSON-encoded string.
    NotJson { value: String },
    /// A failed attempt to convert a native vm [`Value`] to [`Json`] for use outside the vm.
    ToJsonError { error: ToJsonError<S> },
    /// A failed attempt to convert a [`Json`] value into a [`Value`] for use in the vm.
    FromJsonError { error: FromJsonError },
    /// A numeric value took on an invalid value such as NaN.
    NumberError { error: NumberError },
    /// Attempt to use an unsupported feature.
    NotSupported { feature: Feature },
    /// A soft error (e.g., RPC or syscall failure) was promoted to a hard error.
    Promoted { error: String },
    /// A custom error generated explicitly from user code.
    Custom { msg: String }
}
impl<S: System> From<ConversionError<S>> for ErrorCause<S> { fn from(e: ConversionError<S>) -> Self { Self::ConversionError { got: e.got, expected: e.expected } } }
impl<S: System> From<ToJsonError<S>> for ErrorCause<S> { fn from(error: ToJsonError<S>) -> Self { Self::ToJsonError { error } } }
impl<S: System> From<FromJsonError> for ErrorCause<S> { fn from(error: FromJsonError) -> Self { Self::FromJsonError { error } } }
impl<S: System> From<NumberError> for ErrorCause<S> { fn from(error: NumberError) -> Self { Self::NumberError { error } } }

/// A value representing the identity of a [`Value`].
#[derive(Educe)]
#[educe(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
pub struct Identity<'gc, S: System>(*const (), PhantomData<&'gc Value<'gc, S>>);

/// Gets the type of value that is stored.
pub trait GetType {
    type Output: Clone + Copy + PartialEq + Eq + fmt::Debug;
    /// Gets the type of value that is stored.
    fn get_type(&self) -> Self::Output;
}

/// Any primitive value.
#[derive(Educe, Collect)]
#[educe(Clone)]
#[collect(no_drop, bound = "")]
pub enum Value<'gc, S: System> {
    /// A primitive boolean value.
    Bool(#[collect(require_static)] bool),
    /// A primitive numeric value. Snap! and NetsBlox use 64-bit floating point values for all numbers.
    Number(#[collect(require_static)] Number),
    /// A primitive string value, which is an immutable reference type.
    String(#[collect(require_static)] Rc<String>),
    /// An image stored as a binary buffer.
    Image(#[collect(require_static)] Rc<Vec<u8>>),
    /// A reference to a native object handle produced by [`System`].
    Native(#[collect(require_static)] Rc<S::NativeValue>),
    /// A primitive list type, which is a mutable reference type.
    List(GcCell<'gc, VecDeque<Value<'gc, S>>>),
    /// A closure/lambda function. This contains information about the closure's bytecode location, parameters, and captures from the parent scope.
    Closure(GcCell<'gc, Closure<'gc, S>>),
    /// A reference to an [`Entity`] in the environment.
    Entity(GcCell<'gc, Entity<'gc, S>>),
}

impl<'gc, S: System> GetType for Value<'gc, S> {
    type Output = Type<S>;
    fn get_type(&self) -> Self::Output {
        match self {
            Value::Bool(_) => Type::Bool,
            Value::Number(_) => Type::Number,
            Value::String(_) => Type::String,
            Value::Image(_) => Type::Image,
            Value::List(_) => Type::List,
            Value::Closure(_) => Type::Closure,
            Value::Entity(_) => Type::Entity,
            Value::Native(x) => Type::Native(x.get_type()),
        }
    }
}

impl<S: System> fmt::Debug for Value<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn print<'gc, S: System>(value: &Value<'gc, S>, cache: &mut BTreeSet<Identity<'gc, S>>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match value {
                Value::Bool(x) => write!(f, "{x}"),
                Value::Number(x) => write!(f, "{x}"),
                Value::String(x) => write!(f, "{:?}", x.as_str()),
                Value::Closure(x) => write!(f, "{:?}", &*x.read()),
                Value::Entity(x) => write!(f, "{:?}", &*x.read()),
                Value::Native(x) => write!(f, "{:?}", &**x),
                Value::Image(x) => write!(f, "[Image {:?}]", Rc::as_ptr(x)),
                Value::List(x) => {
                    let identity = value.identity();
                    if !cache.insert(identity) { return write!(f, "[...]") }

                    let x = x.read();
                    write!(f, "[")?;
                    for (i, val) in x.iter().enumerate() {
                        print(val, cache, f)?;
                        if i != x.len() - 1 { write!(f, ",")? }
                    }
                    write!(f, "]")?;

                    debug_assert!(cache.contains(&identity));
                    cache.remove(&identity);
                    Ok(())
                }
            }
        }
        let mut cache = Default::default();
        let res = print(self, &mut cache, f);
        if res.is_ok() { debug_assert_eq!(cache.len(), 0); }
        res
    }
}
impl<'gc, S: System> From<bool> for Value<'gc, S> { fn from(v: bool) -> Self { Value::Bool(v) } }
impl<'gc, S: System> From<Number> for Value<'gc, S> { fn from(v: Number) -> Self { Value::Number(v) } }
impl<'gc, S: System> From<Rc<String>> for Value<'gc, S> { fn from(v: Rc<String>) -> Self { Value::String(v) } }
impl<'gc, S: System> From<GcCell<'gc, VecDeque<Value<'gc, S>>>> for Value<'gc, S> { fn from(v: GcCell<'gc, VecDeque<Value<'gc, S>>>) -> Self { Value::List(v) } }
impl<'gc, S: System> From<GcCell<'gc, Closure<'gc, S>>> for Value<'gc, S> { fn from(v: GcCell<'gc, Closure<'gc, S>>) -> Self { Value::Closure(v) } }
impl<'gc, S: System> From<GcCell<'gc, Entity<'gc, S>>> for Value<'gc, S> { fn from(v: GcCell<'gc, Entity<'gc, S>>) -> Self { Value::Entity(v) } }
impl<'gc, S: System> Value<'gc, S> {
    /// Create a new [`Value`] from a [`Json`] value.
    pub fn from_json(mc: MutationContext<'gc, '_>, value: Json) -> Result<Self, FromJsonError> {
        Ok(match value {
            Json::Null => return Err(FromJsonError::HadNull),
            Json::Bool(x) => Value::Bool(x),
            Json::Number(x) => Value::Number(x.as_f64().and_then(|x| Number::new(x).ok()).ok_or(FromJsonError::HadBadNumber)?),
            Json::String(x) => Value::String(Rc::new(x)),
            Json::Array(x) => Value::List(GcCell::allocate(mc, x.into_iter().map(|x| Value::from_json(mc, x)).collect::<Result<_,_>>()?)),
            Json::Object(x) => Value::List(GcCell::allocate(mc, x.into_iter().map(|(k, v)| {
                let mut entry = VecDeque::with_capacity(2);
                entry.push_back(Value::String(Rc::new(k)));
                entry.push_back(Value::from_json(mc, v)?);
                Ok(Value::List(GcCell::allocate(mc, entry)))
            }).collect::<Result<_,_>>()?)),
        })
    }
    /// Converts a [`Value`] into [`Json`]. Note that not all values can be converted to json (e.g., cyclic lists or complex types).
    pub fn to_json(&self) -> Result<Json, ToJsonError<S>> {
        fn simplify<'gc, S: System>(value: &Value<'gc, S>, cache: &mut BTreeSet<Identity<'gc, S>>) -> Result<Json, ToJsonError<S>> {
            Ok(match value {
                Value::Bool(x) => Json::Bool(*x),
                Value::Number(x) => Json::Number(JsonNumber::from_f64(x.get()).ok_or_else(|| ToJsonError::BadNumber(x.get()))?),
                Value::String(x) => Json::String(x.as_str().to_owned()),
                Value::Image(_) | Value::Closure(_) | Value::Entity(_) | Value::Native(_) => return Err(ToJsonError::ComplexType(value.get_type())),
                Value::List(x) => {
                    let identity = value.identity();
                    if !cache.insert(identity) { return Err(ToJsonError::Cyclic) }
                    let res = Json::Array(x.read().iter().map(|x| simplify(x, cache)).collect::<Result<_,_>>()?);
                    debug_assert!(cache.contains(&identity));
                    cache.remove(&identity);
                    res
                }
            })
        }
        let mut cache = Default::default();
        let res = simplify(self, &mut cache);
        if res.is_ok() { debug_assert_eq!(cache.len(), 0); }
        res
    }
    /// Returns a value representing this object that implements [`Eq`] such that
    /// two values are equal if and only if they are references to the same object.
    /// This is primarily useful for testing for reference equality of lists.
    pub fn identity(&self) -> Identity<'gc, S> {
        match self {
            Value::Bool(x) => Identity(x as *const bool as *const (), PhantomData),
            Value::Number(x) => Identity(x as *const Number as *const (), PhantomData),
            Value::String(x) => Identity(Rc::as_ptr(x) as *const (), PhantomData),
            Value::Image(x) => Identity(Rc::as_ptr(x) as *const (), PhantomData),
            Value::List(x) => Identity(x.as_ptr() as *const (), PhantomData),
            Value::Closure(x) => Identity(x.as_ptr() as *const (), PhantomData),
            Value::Entity(x) => Identity(x.as_ptr() as *const (), PhantomData),
            Value::Native(x) => Identity(Rc::as_ptr(x) as *const (), PhantomData),
        }
    }
    /// Attempts to interpret this value as a bool.
    pub fn to_bool(&self) -> Result<bool, ConversionError<S>> {
        Ok(match self {
            Value::Bool(x) => *x,
            x => return Err(ConversionError { got: x.get_type(), expected: Type::Bool }),
        })
    }
    /// Attempts to interpret this value as a number.
    pub fn to_number(&self) -> Result<Number, ConversionError<S>> {
        Ok(match self {
            Value::Number(x) => *x,
            Value::String(x) => x.parse().ok().and_then(|x| Number::new(x).ok()).ok_or(ConversionError { got: Type::String, expected: Type::Number })?,
            x => return Err(ConversionError { got: x.get_type(), expected: Type::Number }),
        })
    }
    /// Attempts to interpret this value as a string.
    pub fn to_string(&self) -> Result<Cow<str>, ConversionError<S>> {
        Ok(match self {
            Value::String(x) => Cow::Borrowed(&*x),
            Value::Number(x) => Cow::Owned(x.to_string()),
            x => return Err(ConversionError { got: x.get_type(), expected: Type::String }),
        })
    }
    /// Attempts to interpret this value as a list.
    pub fn as_list(&self) -> Result<GcCell<'gc, VecDeque<Value<'gc, S>>>, ConversionError<S>> {
        match self {
            Value::List(x) => Ok(*x),
            x => Err(ConversionError { got: x.get_type(), expected: Type::List }),
        }
    }
    /// Attempts to interpret this value as a closure.
    pub fn as_closure(&self) -> Result<GcCell<'gc, Closure<'gc, S>>, ConversionError<S>> {
        match self {
            Value::Closure(x) => Ok(*x),
            x => Err(ConversionError { got: x.get_type(), expected: Type::Closure }),
        }
    }
    /// Attempts to interpret this value as an entity.
    pub fn as_entity(&self) -> Result<GcCell<'gc, Entity<'gc, S>>, ConversionError<S>> {
        match self {
            Value::Entity(x) => Ok(*x),
            x => Err(ConversionError { got: x.get_type(), expected: Type::Entity }),
        }
    }
}

/// Information about a closure/lambda function.
#[derive(Collect)]
#[collect(no_drop, bound = "")]
pub struct Closure<'gc, S: System> {
    #[collect(require_static)] pub pos: usize,
    #[collect(require_static)] pub params: Vec<String>,
                               pub captures: SymbolTable<'gc, S>,
}
impl<S: System> fmt::Debug for Closure<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Closure {:#08x}", self.pos)
    }
}

/// The kind of entity being represented.
pub enum EntityKind<'gc, 'a, S: System> {
    Stage,
    Sprite,
    SpriteClone { parent: &'a Entity<'gc, S> },
}

/// Information about an entity (sprite or stage).
#[derive(Collect)]
#[collect(no_drop, bound = "")]
pub struct Entity<'gc, S: System> {
    #[collect(require_static)] pub name: String,
                               pub fields: SymbolTable<'gc, S>,
    #[collect(require_static)] pub state: S::EntityState,
}
impl<S: System> fmt::Debug for Entity<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Entity {:?}", self.name)
    }
}

/// Represents a shared mutable resource.
/// 
/// This is effectively equivalent to [`GcCell<T>`] except that it performs no dynamic allocation
/// for the [`Shared::Unique`] case, which is assumed to be significantly more likely than [`Shared::Aliased`].
#[derive(Collect)]
#[collect(no_drop)]
pub enum Shared<'gc, T: 'gc + Collect> {
    /// A shared resource which has only (this) single unique handle.
    Unique(T),
    /// One of several handles to a single shared resource.
    Aliased(GcCell<'gc, T>),
}
impl<'gc, T: 'gc + Collect> Shared<'gc, T> {
    /// Sets the value of the shared resource.
    pub fn set(&mut self, mc: MutationContext<'gc, '_>, value: T) {
        match self {
            Shared::Unique(x) => *x = value,
            Shared::Aliased(x) => *x.write(mc) = value,
        }
    }
    /// Gets a reference to the shared resource's currently stored value.
    pub fn get(&self) -> SharedRef<T> {
        match self {
            Shared::Unique(x) => SharedRef::Unique(x),
            Shared::Aliased(x) => SharedRef::Aliased(x.read()),
        }
    }
    /// Creates an aliasing instance of [`Shared`] to the same resource as this one.
    /// If this instance is the [`Shared::Unique`] variant, transitions to [`Shared::Aliased`] and returns a second handle.
    /// Otherwise, this simple returns an additional handle to the aliased shared resource.
    pub fn alias(&mut self, mc: MutationContext<'gc, '_>) -> Self {
        take_mut::take(self, |myself| {
            match myself {
                Shared::Unique(x) => Shared::Aliased(GcCell::allocate(mc, x)),
                Shared::Aliased(_) => myself,
            }
        });

        match self {
            Shared::Unique(_) => unreachable!(),
            Shared::Aliased(x) => Shared::Aliased(*x),
        }
    }
}
impl<'gc, T: Collect> From<T> for Shared<'gc, T> { fn from(value: T) -> Self { Shared::Unique(value) } }

pub enum SharedRef<'a, T> {
    Unique(&'a T),
    Aliased(Ref<'a, T>)
}
impl<'a, T> Deref for SharedRef<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        match self {
            SharedRef::Unique(x) => x,
            SharedRef::Aliased(x) => &**x,
        }
    }
}

/// Holds a collection of variables in an execution context.
/// 
/// [`SymbolTable`] has utilities to extract variables from an abstract syntax tree, or to explicitly define variables.
/// Simple methods are provided to perform value lookups in the table.
#[derive(Collect)]
#[collect(no_drop, bound = "")]
pub struct SymbolTable<'gc, S: System>(BTreeMap<String, Shared<'gc, Value<'gc, S>>>);
impl<'gc, S: System> Default for SymbolTable<'gc, S> { fn default() -> Self { Self(Default::default()) } }
impl<'gc, S: System> SymbolTable<'gc, S> {
    /// Sets the value of an existing variable (as if by [`Shared::set`]) or defines it if it does not exist.
    /// If the variable does not exist, creates a [`Shared::Unique`] instance for the new `value`.
    /// If you would prefer to always create a new, non-aliased value, consider using [`SymbolTable::redefine_or_define`] instead.
    pub fn set_or_define(&mut self, mc: MutationContext<'gc, '_>, var: &str, value: Value<'gc, S>) {
        match self.0.get_mut(var) {
            Some(x) => x.set(mc, value),
            None => { self.0.insert(var.to_owned(), value.into()); }
        }
    }
    /// Defines or redefines a value in the symbol table to a new instance of [`Shared<Value>`].
    /// Note that this is not the same as [`SymbolTable::set_or_define`], which sets a value on a potentially aliased variable.
    /// If a variable named `var` already existed and was [`Shared::Aliased`], its value is not modified.
    pub fn redefine_or_define(&mut self, var: &str, value: Shared<'gc, Value<'gc, S>>) {
        self.0.insert(var.to_owned(), value);
    }
    /// Looks up the given variable in the symbol table.
    /// If a variable with the given name does not exist, returns [`None`].
    pub fn lookup(&self, var: &str) -> Option<&Shared<'gc, Value<'gc, S>>> {
        self.0.get(var)
    }
    /// Equivalent to [`SymbolTable::lookup`] except that it returns a mutable reference.
    pub fn lookup_mut(&mut self, var: &str) -> Option<&mut Shared<'gc, Value<'gc, S>>> {
        self.0.get_mut(var)
    }
    /// Gets the number of symbols currently stored in the symbol table.
    pub fn len(&self) -> usize {
        self.0.len()
    }
    /// Checks if the symbol table is currently empty (no defined symbols).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Iterates over the key value pairs stored in the symbol table.
    pub fn iter(&self) -> symbol_table::Iter<'gc, '_, S> {
        symbol_table::Iter(self.0.iter())
    }
    /// Iterates over the key value pairs stored in the symbol table.
    pub fn iter_mut(&mut self) -> symbol_table::IterMut<'gc, '_, S> {
        symbol_table::IterMut(self.0.iter_mut())
    }
}
impl<'gc, S: System> IntoIterator for SymbolTable<'gc, S> {
    type Item = (String, Shared<'gc, Value<'gc, S>>);
    type IntoIter = symbol_table::IntoIter<'gc, S>;
    fn into_iter(self) -> Self::IntoIter { symbol_table::IntoIter(self.0.into_iter()) }
}
impl<'gc, 'a, S: System> IntoIterator for &'a SymbolTable<'gc, S> {
    type Item = <symbol_table::Iter<'gc, 'a, S> as Iterator>::Item;
    type IntoIter = symbol_table::Iter<'gc, 'a, S>;
    fn into_iter(self) -> Self::IntoIter { self.iter() }
}
impl<'gc, 'a, S: System> IntoIterator for &'a mut SymbolTable<'gc, S> {
    type Item = <symbol_table::IterMut<'gc, 'a, S> as Iterator>::Item;
    type IntoIter = symbol_table::IterMut<'gc, 'a, S>;
    fn into_iter(self) -> Self::IntoIter { self.iter_mut() }
}
pub mod symbol_table {
    //! Special types for working with a [`SymbolTable`].
    use super::*;
    pub struct IntoIter<'gc, S: System>(pub(crate) std::collections::btree_map::IntoIter<String, Shared<'gc, Value<'gc, S>>>);
    pub struct Iter<'gc, 'a, S: System>(pub(crate) std::collections::btree_map::Iter<'a, String, Shared<'gc, Value<'gc, S>>>);
    pub struct IterMut<'gc, 'a, S: System>(pub(crate) std::collections::btree_map::IterMut<'a, String, Shared<'gc, Value<'gc, S>>>);
    impl<'gc, S: System> Iterator for IntoIter<'gc, S> { type Item = (String, Shared<'gc, Value<'gc, S>>); fn next(&mut self) -> Option<Self::Item> { self.0.next() } }
    impl<'gc, 'a, S: System> Iterator for Iter<'gc, 'a, S> { type Item = (&'a String, &'a Shared<'gc, Value<'gc, S>>); fn next(&mut self) -> Option<Self::Item> { self.0.next() } }
    impl<'gc, 'a, S: System> Iterator for IterMut<'gc, 'a, S> { type Item = (&'a String, &'a mut Shared<'gc, Value<'gc, S>>); fn next(&mut self) -> Option<Self::Item> { self.0.next() } }
}

/// A collection of symbol tables with hierarchical context searching.
pub(crate) struct LookupGroup<'gc, 'a, 'b, S: System>(&'a mut [&'b mut SymbolTable<'gc, S>]);
impl<'gc, 'a, 'b, S: System> LookupGroup<'gc, 'a, 'b, S> {
    /// Creates a new lookup group.
    /// The first symbol table is intended to be the most-global, and subsequent tables are increasingly more-local.
    pub fn new(tables: &'a mut [&'b mut SymbolTable<'gc, S>]) -> Self {
        debug_assert!(!tables.is_empty());
        Self(tables)
    }
    /// Searches for the given variable in this group of lookup tables,
    /// starting with the last (most-local) table and working towards the first (most-global) table.
    /// Returns a reference to the value if it is found, otherwise returns [`None`].
    pub fn lookup(&self, var: &str) -> Option<&Shared<'gc, Value<'gc, S>>> {
        for src in self.0.iter().rev() {
            if let Some(val) = src.lookup(var) {
                return Some(val);
            }
        }
        None
    }
    /// As [`LookupGroup::lookup`], but returns a mutable reference.
    pub fn lookup_mut(&mut self, var: &str) -> Option<&mut Shared<'gc, Value<'gc, S>>> {
        for src in self.0.iter_mut().rev() {
            if let Some(val) = src.lookup_mut(var) {
                return Some(val);
            }
        }
        None
    }
    /// Performs a lookup for the given variable.
    /// If it already exists, assigns it a new value.
    /// Otherwise, defines it in the last (most-local) context equivalently to [`SymbolTable::set_or_define`].
    pub fn set_or_define(&mut self, mc: MutationContext<'gc, '_>, var: &str, value: Value<'gc, S>) {
        match self.lookup_mut(var) {
            Some(x) => x.set(mc, value),
            None => self.0.last_mut().unwrap().set_or_define(mc, var, value),
        }
    }
    /// Gets a mutable reference to the last (most-local) context.
    pub fn locals_mut(&mut self) -> &mut SymbolTable<'gc, S> {
        self.0.last_mut().unwrap()
    }
}

/// The error promotion paradigm to use for certain types of runtime errors.
#[derive(Clone, Copy)]
pub enum ErrorScheme {
    /// Emit errors as soft errors. This causes the error message to be returned as a [`Value::String`] object,
    /// as well as being stored in a corresponding last-error process-local variable.
    Soft,
    /// Emit errors as hard errors. This treats certain classes of typically soft errors as hard errors that
    /// must be caught or else terminate the [`Process`](crate::process::Process) (not the entire VM).
    Hard,
}

/// Settings to use for a [`Process`](crate::process::Process).
#[derive(Clone, Copy)]
pub struct Settings {
    /// The maximum depth of the call stack (default `1024`).
    pub max_call_depth: usize,
    /// The error pattern to use for rpc errors (default [`ErrorScheme::Hard`]).
    pub rpc_error_scheme: ErrorScheme,
    /// The error pattern to use for syscall errors (default [`ErrorScheme::Hard`]).
    pub syscall_error_scheme: ErrorScheme,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            max_call_depth: 1024,
            rpc_error_scheme: ErrorScheme::Hard,
            syscall_error_scheme: ErrorScheme::Hard,
        }
    }
}

/// Global information about the execution state of an entire project.
#[derive(Collect)]
#[collect(no_drop, bound = "")]
pub struct GlobalContext<'gc, S: System> {
    #[collect(require_static)] pub bytecode: Rc<ByteCode>,
    #[collect(require_static)] pub settings: Settings,
    #[collect(require_static)] pub system: Rc<S>,
    #[collect(require_static)] pub timer_start: u64,
    #[collect(require_static)] pub proj_name: String,
                               pub globals: SymbolTable<'gc, S>,
                               pub entities: BTreeMap<String, GcCell<'gc, Entity<'gc, S>>>,
}
impl<'gc, S: System> GlobalContext<'gc, S> {
    pub fn from_init(mc: MutationContext<'gc, '_>, init_info: &InitInfo, bytecode: Rc<ByteCode>, settings: Settings, system: Rc<S>) -> Self {
        let allocated_refs = init_info.ref_values.iter().map(|ref_value| match ref_value {
            RefValue::String(value) => Value::String(Rc::new(value.clone())),
            RefValue::List(_) => Value::List(GcCell::allocate(mc, Default::default())),
        }).collect::<Vec<_>>();

        fn get_value<'gc, S: System>(value: &InitValue, allocated_refs: &Vec<Value<'gc, S>>) -> Value<'gc, S> {
            match value {
                InitValue::Bool(x) => Value::Bool(*x),
                InitValue::Number(x) => Value::Number(*x),
                InitValue::Ref(x) => allocated_refs[*x].clone(),
            }
        }

        for (allocated_ref, ref_value) in iter::zip(&allocated_refs, &init_info.ref_values) {
            match ref_value {
                RefValue::String(_) => continue,
                RefValue::List(values) => {
                    let allocated_ref = match allocated_ref {
                        Value::List(x) => x,
                        _ => unreachable!(),
                    };
                    let mut allocated_ref = allocated_ref.write(mc);
                    for value in values {
                        allocated_ref.push_back(get_value(value, &allocated_refs));
                    }
                }
            }
        }

        let mut globals = SymbolTable::default();
        for (global, value) in init_info.globals.iter() {
            globals.redefine_or_define(global, Shared::Unique(get_value(value, &allocated_refs)));
        }

        let mut entities = BTreeMap::new();
        for (i, entity_info) in init_info.entities.iter().enumerate() {
            let kind = if i == 0 { EntityKind::Stage } else { EntityKind::Sprite };
            let name = entity_info.name.clone();
            let state = kind.into();

            let mut fields = SymbolTable::default();
            for (field, value) in entity_info.fields.iter() {
                fields.redefine_or_define(field, Shared::Unique(get_value(value, &allocated_refs)));
            }

            entities.insert(name.clone(), GcCell::allocate(mc, Entity { name, fields, state }));
        }

        let proj_name = init_info.proj_name.clone();
        let timer_start = system.time_ms().unwrap_or(0);

        Self { proj_name, globals, entities, timer_start, system, settings, bytecode }
    }
}

/// A blocking handle for a [`BarrierCondition`].
#[derive(Debug, Default, Clone)]
pub struct Barrier(Rc<()>);
/// Waits for the destruction of all associated [`Barrier`] handles.
#[derive(Debug, Clone)]
pub struct BarrierCondition(Weak<()>);
impl Barrier {
    /// Creates a new [`Barrier`] which is not related to any other barrier.
    /// A barrier can be cloned to create additional associated, blocking handles for the same condition.
    pub fn new() -> Self {
        Barrier(Rc::new(()))
    }
    /// Constructs a [`BarrierCondition`] object which waits for this barrier handle and all of its associated handles
    /// (created before or after this point) to be destroyed.
    pub fn get_condition(&self) -> BarrierCondition {
        BarrierCondition(Rc::downgrade(&self.0))
    }
}
impl BarrierCondition {
    /// Checks if the condition has been completed, i.e., that all the associated barriers have been destroyed.
    pub fn is_completed(&self) -> bool {
        self.0.strong_count() == 0
    }
}

/// The result of an operation that might be synchronous or asynchronous.
pub enum MaybeAsync<T, K> {
    /// A synchronous result with a return value of type `T`.
    Sync(T),
    /// An asynchronous result with the given async key type `K`,
    /// which is expected to be usable to later obtain an [`AsyncPoll<T>`].
    Async(K),
}

/// The result of a successful call to an async poller operation such as in [`System`].
pub enum AsyncPoll<T> {
    /// The async operation completed with the given value.
    Completed(T),
    /// The async operation is still pending and has not completed.
    Pending,
}

/// Types of [`System`] resources, grouped into feature categories.
#[derive(Debug)]
pub enum Feature {
    /// The ability of a process to generate random numbers.
    Random,
    /// The ability of a process to get the current time (not necessarily wall time).
    Time,

    /// The ability of a process to request keyboard input from the user.
    Input,
    /// The ability of a process to perform a syscall of the given name.
    Syscall { name: String },
    /// The ability of a process to perform an RPC call.
    Rpc { service: String, rpc: String },
    /// The ability of an entity to know its position.
    Position,
    /// The ability of an entity to know its heading.
    Heading,

    /// The ability of a process to display information.
    Print,
    /// The ability of an entity to move forward or backwards by a distance.
    Forward,
    /// The ability of an entity to turn right or left by a given angle.
    Turn,
}

/// A value-returning request issued from the runtime.
pub enum Request<'gc, S: System> {
    /// Request input from the user. The `prompt` argument is either [`Some`] prompt to display, or [`None`] for no prompt.
    Input { prompt: Option<Value<'gc, S>> },
    /// Performs a system call on the local hardware to access device resources.
    Syscall { name: String, args: Vec<Value<'gc, S>> },
    /// Requests the system to execute the given RPC.
    Rpc { service: String, rpc: String, args: Vec<(String, Value<'gc, S>)> },
    /// Request to get the position of an entity. This should return a list of two numbers.
    Position,
    /// Request to get the heading of an entity. This should return a number.
    Heading,
}
impl<'gc, S: System> Request<'gc, S> {
    /// Gets the [`Feature`] associated with this request.
    pub fn feature(&self) -> Feature {
        match self {
            Request::Input { .. } => Feature::Input,
            Request::Syscall { name, .. } => Feature::Syscall { name: name.clone() },
            Request::Rpc { service, rpc, .. } => Feature::Rpc { service: service.clone(), rpc: rpc.clone() },
            Request::Position => Feature::Position,
            Request::Heading => Feature::Heading,
        }
    }
}

/// A non-value-returning command issued from the runtime.
pub enum Command<'gc, S: System> {
    /// Output [`Some`] [`Value`] or [`None`] to perform a Snap!-style clear.
    Print { value: Option<Value<'gc, S>> },
    /// Move forward by a given distance. If the distance is negative, move backwards instead.
    Forward { distance: Number },
    /// Turn right by a given angle. If the angle is negative, turn left instead.
    Turn { angle: Number },
}
impl<'gc, S: System> Command<'gc, S> {
    /// Gets the [`Feature`] associated with this command.
    pub fn feature(&self) -> Feature {
        match self {
            Command::Print { .. } => Feature::Print,
            Command::Forward { .. } => Feature::Forward,
            Command::Turn { .. } => Feature::Turn,
        }
    }
}

/// The status of a potentially-handled request.
pub enum RequestStatus<'gc, S: System> {
    /// The request was handled by the overriding client.
    Handled,
    /// The request was not handled by the overriding client,
    /// and the default system implementation should be used instead.
    UseDefault { key: S::RequestKey, request: Request<'gc, S> },
}
/// The status of a potentially-handled command.
pub enum CommandStatus<'gc, S: System> {
    /// The command was handled by the overriding client.
    Handled,
    /// The command was not handled by the overriding client,
    /// and the default system implementation should be used instead.
    UseDefault { key: S::CommandKey, command: Command<'gc, S> },
}

/// A collection of implementation options that could be used for implementing a customizable [`System`].
#[derive(Educe)]
#[educe(Default, Clone)]
pub struct Config<S: System> {
    /// A function used to perform asynchronous requests that yield a value back to the runtime.
    pub request: Option<Rc<dyn for<'gc> Fn(&S, MutationContext<'gc, '_>, S::RequestKey, Request<'gc, S>, &Entity<'gc, S>) -> RequestStatus<'gc, S>>>,
    /// A function used to perform asynchronous tasks whose completion is awaited by the runtime.
    pub command: Option<Rc<dyn for<'gc> Fn(&S, MutationContext<'gc, '_>, S::CommandKey, Command<'gc, S>, &Entity<'gc, S>) -> CommandStatus<'gc, S>>>,
}
impl<S: System> Config<S> {
    /// Composes two [`Config`] objects, prioritizing the implementation of `self`.
    pub fn fallback(&self, other: &Self) -> Self {
        Self {
            request: match (self.request.clone(), other.request.clone()) {
                (Some(a), Some(b)) => Some(Rc::new(move |system, mc, key, request, entity| {
                    match a(system, mc, key, request, entity) {
                        RequestStatus::Handled => RequestStatus::Handled,
                        RequestStatus::UseDefault { key, request } => b(system, mc, key, request, entity),
                    }
                })),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            },
            command: match (self.command.clone(), other.command.clone()) {
                (Some(a), Some(b)) => Some(Rc::new(move |system, mc, key, command, entity| {
                    match a(system, mc, key, command, entity) {
                        CommandStatus::Handled => CommandStatus::Handled,
                        CommandStatus::UseDefault { key, command } => b(system, mc, key, command, entity),
                    }
                })),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            },
        }
    }
}

/// Represents all the features of an implementing system.
/// 
/// This type encodes any features that cannot be performed without platform-specific resources.
/// 
/// When implementing [`System`] for some type, you may prefer to not support one or more features.
/// This can be accomplished by returning the [`ErrorCause::NotSupported`] variant for the relevant [`Feature`].
pub trait System: 'static + Sized {
    /// A type representing native values that the system can operate on or return through syscalls.
    /// This could, for example, be used to allow a process to hold on to a file handle stored in a variable.
    /// If multiple native types are required, an enum can be used.
    /// Native types have reference semantics in the vm, just like for entities.
    type NativeValue: 'static + GetType + fmt::Debug;

    /// Key type used to await the result of an asynchronous request.
    type RequestKey: 'static;
    /// Key type used to await the completion of an asynchronous command.
    type CommandKey: 'static;

    /// Key type used to await the result of a "send message and wait" block (response from target).
    type ExternReplyKey: 'static;
    /// Key type used to reply to a message that was sent to this client with the expectation of receiving a response.
    /// This type is required to be [`Clone`] because there can be multiple message handlers for the same message type.
    type InternReplyKey: 'static + Clone;

    /// Type used to represent an entity's system-specific state.
    /// This should include any details outside of core process functionality (e.g., graphics, position, orientation).
    /// This type should be constructable from [`EntityKind`], which is used to initialize a new entity in the runtime.
    type EntityState: 'static + for<'gc, 'a> From<EntityKind<'gc, 'a, Self>>;

    /// Gets a random value sampled from the given `range`, which is assumed to be non-empty.
    /// The input for this generic function is such that it is compatible with [`rand::Rng::gen_range`],
    /// which makes it possible to implement this function with any random provider under the [`rand`] crate standard.
    fn rand<T, R>(&self, range: R) -> Result<T, ErrorCause<Self>> where T: SampleUniform, R: SampleRange<T>;

    /// Gets the current time in milliseconds.
    /// This is not required to represent the actual real-world time; e.g., this could simply measure uptime.
    /// Subsequent values are required to be non-decreasing.
    fn time_ms(&self) -> Result<u64, ErrorCause<Self>>;

    /// Performs a general request which returns a value to the system.
    /// Ideally, this function should be non-blocking, and the requestor will await the result asynchronously.
    /// The [`Entity`] that made the request is provided for context.
    fn perform_request<'gc>(&self, mc: MutationContext<'gc, '_>, request: Request<'gc, Self>, entity: &Entity<'gc, Self>) -> Result<MaybeAsync<Result<Value<'gc, Self>, String>, Self::RequestKey>, ErrorCause<Self>>;
    /// Poll for the completion of an asynchronous request.
    /// The [`Entity`] that made the request is provided for context.
    fn poll_request<'gc>(&self, mc: MutationContext<'gc, '_>, key: &Self::RequestKey, entity: &Entity<'gc, Self>) -> Result<AsyncPoll<Result<Value<'gc, Self>, String>>, ErrorCause<Self>>;

    /// Performs a general command which does not return a value to the system.
    /// Ideally, this function should be non-blocking, and the commander will await the task's completion asynchronously.
    /// The [`Entity`] that issued the command is provided for context.
    fn perform_command<'gc>(&self, mc: MutationContext<'gc, '_>, command: Command<'gc, Self>, entity: &Entity<'gc, Self>) -> Result<MaybeAsync<Result<(), String>, Self::CommandKey>, ErrorCause<Self>>;
    /// Poll for the completion of an asynchronous command.
    /// The [`Entity`] that issued the command is provided for context.
    fn poll_command<'gc>(&self, mc: MutationContext<'gc, '_>, key: &Self::CommandKey, entity: &Entity<'gc, Self>) -> Result<AsyncPoll<Result<(), String>>, ErrorCause<Self>>;

    /// Sends a message containing a set of named `values` to each of the specified `targets`.
    /// The `expect_reply` value controls whether or not to use a reply mechanism to asynchronously receive a response from the target(s).
    /// In the case that there are multiple targets, only the first reply (if any) should be used.
    fn send_message(&self, msg_type: String, values: Vec<(String, Json)>, targets: Vec<String>, expect_reply: bool) -> Result<Option<Self::ExternReplyKey>, ErrorCause<Self>>;
    /// Polls for a response from a client initiated by [`System::send_message`].
    /// If the client responds, a value of [`Some(x)`] is returned.
    /// The system may elect to impose a timeout for reply results, in which case [`None`] is returned instead.
    fn poll_reply(&self, key: &Self::ExternReplyKey) -> AsyncPoll<Option<Json>>;
    /// Attempts to receive a message from the message buffer.
    /// This operation is always non-blocking and returns [`None`] if there are no messages in the buffer.
    /// If a message is received, a tuple of form `(msg_type, values, reply_key)` is returned.
    fn receive_message(&self) -> Option<(String, Vec<(String, Json)>, Option<Self::InternReplyKey>)>;
    /// Sends a reply to the sender of a blocking message this client received.
    fn send_reply(&self, key: Self::InternReplyKey, value: Json) -> Result<(), ErrorCause<Self>>;
}
