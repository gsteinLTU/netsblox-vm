use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::rc::{Rc, Weak};
use alloc::borrow::{Cow, ToOwned};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use core::marker::PhantomData;
use core::{iter, fmt, mem};
use core::ops::Deref;
use core::cell::Ref;

use rand::distributions::uniform::{SampleUniform, SampleRange};
use checked_float::{FloatChecker, CheckedFloat};

#[cfg(feature = "serde")]
use serde::{Serialize, Deserialize};

use crate::*;
use crate::gc::*;
use crate::json::*;
use crate::real_time::*;
use crate::bytecode::*;

/// Error type used by [`NumberChecker`].
#[derive(Debug)]
pub enum NumberError {
    Nan,
}

/// [`FloatChecker`] type used for validating a [`Number`].
pub struct NumberChecker;
impl FloatChecker<f64> for NumberChecker {
    type Error = NumberError;
    fn check(value: f64) -> Result<(), Self::Error> {
        if value.is_nan() { return Err(NumberError::Nan); }
        Ok(())
    }
}

/// The type used to represent numbers in the runtime.
pub type Number = CheckedFloat<f64, NumberChecker>;

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
pub enum ToJsonError<C: CustomTypes<S>, S: System<C>> {
    /// The value was or contained a number which could not be expressed as [`f64`].
    BadNumber(f64),
    /// The value was or contained a type that cannot be exported as primitive [`Json`].
    ComplexType(Type<C, S>),
    /// The value contained a cycle, which [`Json`] forbids.
    Cyclic,
}

/// The type of a [`Value`].
#[derive(Educe)]
#[educe(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type<C: CustomTypes<S>, S: System<C>> {
    Bool, Number, String, Image, Audio, List, Closure, Entity, Native(<C::NativeValue as GetType>::Output),
}

/// A type conversion error on a [`Value`].
#[derive(Educe)]
#[educe(Debug)]
pub struct ConversionError<C: CustomTypes<S>, S: System<C>> {
    pub got: Type<C, S>,
    pub expected: Type<C, S>,
}

/// The cause/explanation of an execution error.
#[derive(Educe)]
#[educe(Debug)]
pub enum ErrorCause<C: CustomTypes<S>, S: System<C>> {
    /// A variable lookup operation failed. `name` holds the name of the variable that was expected.
    UndefinedVariable { name: String },
    /// A name-based costume lookup operation failed.
    UndefinedCostume { name: String },
    /// A name-based entity lookup operation failed.
    UndefinedEntity { name: String },
    /// An upvar was created at the root scope, which is not allowed (it has nothing to refer up to).
    UpvarAtRoot,
    /// The result of a failed type conversion.
    ConversionError { got: Type<C, S>, expected: Type<C, S> },
    /// The result of a failed variadic type conversion (expected type `T` or a list of type `T`).
    VariadicConversionError { got: Type<C, S>, expected: Type<C, S> },
    /// Attempt to compare incomparable types.
    Incomparable { left: Type<C, S>, right: Type<C, S> },
    /// An operation that expected a non-empty list received an empty list as input.
    EmptyList,
    /// An operation that expected a list with a certain size received an incorrect size.
    InvalidListLength { expected: usize, got: usize },
    /// An indexing operation on a list/string had an out of bounds index, `index`, on a list/string of size `len`. Note that Snap!/NetsBlox use 1-based indexing.
    IndexOutOfBounds { index: i64, len: usize },
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
    /// Attempt to parse an invalid CSV-encoded string.
    NotCsv { value: String },
    /// Attempt to parse an invalid JSON-encoded string.
    NotJson { value: String },
    /// A failed attempt to convert a native vm [`Value`] to [`Json`] for use outside the vm.
    ToJsonError { error: ToJsonError<C, S> },
    /// A failed attempt to convert a [`Json`] value into a [`Value`] for use in the vm.
    FromJsonError { error: FromJsonError },
    /// A numeric value took on an invalid value such as NaN.
    NumberError { error: NumberError },
    /// Attempt to use an unsupported feature.
    NotSupported { feature: Feature },
    /// A soft error (e.g., RPC or syscall failure) was promoted to a hard error.
    Promoted { error: String },
    /// A custom error generated explicitly from user code.
    Custom { msg: String },
}
impl<C: CustomTypes<S>, S: System<C>> From<ConversionError<C, S>> for ErrorCause<C, S> { fn from(e: ConversionError<C, S>) -> Self { Self::ConversionError { got: e.got, expected: e.expected } } }
impl<C: CustomTypes<S>, S: System<C>> From<ToJsonError<C, S>> for ErrorCause<C, S> { fn from(error: ToJsonError<C, S>) -> Self { Self::ToJsonError { error } } }
impl<C: CustomTypes<S>, S: System<C>> From<FromJsonError> for ErrorCause<C, S> { fn from(error: FromJsonError) -> Self { Self::FromJsonError { error } } }
impl<C: CustomTypes<S>, S: System<C>> From<NumberError> for ErrorCause<C, S> { fn from(error: NumberError) -> Self { Self::NumberError { error } } }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Color { pub r: u8, pub g: u8, pub b: u8, pub a: u8 }
impl Color {
    pub fn from_hsva(mut h: f32, mut s: f32, mut v: f32, mut a: f32) -> Self {
        h = h.rem_euclid(360.0);
        s = s.clamp(0.0, 1.0);
        v = v.clamp(0.0, 1.0);
        a = a.clamp(0.0, 1.0);

        let c = v * s;
        let hp = h / 60.0;
        let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
        let m = v - c;

        let (r, g, b) = match hp as usize {
            0 | 6 => (c, x, 0.0), // (0 mod 6) - needed because rem_euclid is not perfect
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            5 => (c, 0.0, x),
            _ => unreachable!(),
        };

        fn f(x: f32) -> u8 { (x * 255.0).round() as u8 }

        Self { r: f(r + m), g: f(g + m), b: f(b + m), a: f(a) }
    }
    pub fn to_hsva(self) -> (f32, f32, f32, f32) {
        fn f(x: u8) -> f32 { x as f32 / 255.0 }

        let vals = [self.r, self.g, self.b];
        let (c_max_i, c_max) = vals.iter().copied().enumerate().max_by_key(|x| x.1).map(|(i, v)| (i, f(v))).unwrap();
        let c_min = vals.iter().copied().min().map(f).unwrap();
        let delta = c_max - c_min;

        let h = if delta == 0.0 { 0.0 } else {
            match c_max_i {
                0 => 60.0 * ((f(self.g) - f(self.b)) / delta).rem_euclid(6.0),
                1 => 60.0 * ((f(self.b) - f(self.r)) / delta + 2.0),
                2 => 60.0 * ((f(self.r) - f(self.g)) / delta + 4.0),
                _ => unreachable!(),
            }
        };
        let s = if c_max == 0.0 { 0.0 } else { delta / c_max };
        let v = c_max;
        let a = f(self.a);

        (h, s, v, a)
    }
}

#[test]
fn test_color_hsv_to_rgb() {
    assert_eq!(Color::from_hsva(0.0, 0.0, 0.0, 1.0), Color { r: 0x00, g: 0x00, b: 0x00, a: 0xFF });
    assert_eq!(Color::from_hsva(0.0, -0.5, 0.0, 1.0), Color { r: 0x00, g: 0x00, b: 0x00, a: 0xFF });
    assert_eq!(Color::from_hsva(0.0, 0.07, 0.36, 1.0), Color { r: 0x5C, g: 0x55, b: 0x55, a: 0xFF });
    assert_eq!(Color::from_hsva(0.0, 1.0, 0.36, 1.0), Color { r: 92, g: 0, b: 0, a: 0xFF });
    assert_eq!(Color::from_hsva(0.0, 1.5, 0.36, 1.0), Color { r: 92, g: 0, b: 0, a: 0xFF });
    assert_eq!(Color::from_hsva(0.0, 1.3, 0.36, 1.0), Color { r: 92, g: 0, b: 0, a: 0xFF });
    assert_eq!(Color::from_hsva(0.0, 14.5, 0.36, 1.0), Color { r: 92, g: 0, b: 0, a: 0xFF });
    assert_eq!(Color::from_hsva(0.0, 0.0, 0.36, 1.0), Color { r: 92, g: 92, b: 92, a: 0xFF });
    assert_eq!(Color::from_hsva(0.0, -2.4, 0.36, 1.0), Color { r: 92, g: 92, b: 92, a: 0xFF });
    assert_eq!(Color::from_hsva(0.0, -0.4, 0.36, 1.0), Color { r: 92, g: 92, b: 92, a: 0xFF });
    assert_eq!(Color::from_hsva(360.0, 0.07, 0.36, 1.0), Color { r: 0x5C, g: 0x55, b: 0x55, a: 0xFF });
    assert_eq!(Color::from_hsva(-360.0, 0.07, 0.36, 1.0), Color { r: 0x5C, g: 0x55, b: 0x55, a: 0xFF });
    assert_eq!(Color::from_hsva(25.0, 0.5, 0.25, 1.0), Color { r: 0x40, g: 0x2D, b: 0x20, a: 0xFF });
    assert_eq!(Color::from_hsva(25.0 + 360.0, 0.5, 0.25, 1.0), Color { r: 0x40, g: 0x2D, b: 0x20, a: 0xFF });
    assert_eq!(Color::from_hsva(25.0 - 360.0, 0.5, 0.25, 1.0), Color { r: 0x40, g: 0x2D, b: 0x20, a: 0xFF });
    assert_eq!(Color::from_hsva(49.0, 0.75, 0.12, 1.0), Color { r: 0x1F, g: 0x1A, b: 0x08, a: 0xFF });
    assert_eq!(Color::from_hsva(65.0, 0.12, 0.87, 1.0), Color { r: 0xDC, g: 0xDE, b: 0xC3, a: 0xFF });
    assert_eq!(Color::from_hsva(65.0, 0.12, 1.0, 1.0), Color { r: 252, g: 255, b: 224, a: 0xFF });
    assert_eq!(Color::from_hsva(65.0, 0.12, 1.4, 1.0), Color { r: 252, g: 255, b: 224, a: 0xFF });
    assert_eq!(Color::from_hsva(90.0, 0.22, 0.55, 1.0), Color { r: 0x7D, g: 0x8C, b: 0x6D, a: 0xFF });
    assert_eq!(Color::from_hsva(90.0 + 360.0, 0.22, 0.55, 1.0), Color { r: 0x7D, g: 0x8C, b: 0x6D, a: 0xFF });
    assert_eq!(Color::from_hsva(90.0, 0.22, 0.55, 1.0), Color { r: 0x7D, g: 0x8C, b: 0x6D, a: 0xFF });
    assert_eq!(Color::from_hsva(120.0, 0.26, 0.91, 1.0), Color { r: 0xAC, g: 0xE8, b: 0xAC, a: 0xFF });
    assert_eq!(Color::from_hsva(175.0, 0.97, 0.04, 1.0), Color { r: 0x00, g: 0x0A, b: 0x09, a: 0xFF });
    assert_eq!(Color::from_hsva(175.0 + 360.0, 0.97, 0.04, 1.0), Color { r: 0x00, g: 0x0A, b: 0x09, a: 0xFF });
    assert_eq!(Color::from_hsva(175.0 - 360.0, 0.97, 0.04, 1.0), Color { r: 0x00, g: 0x0A, b: 0x09, a: 0xFF });
    assert_eq!(Color::from_hsva(180.0, 1.0, 1.0, 1.0), Color { r: 0x00, g: 0xFF, b: 0xFF, a: 0xFF });
    assert_eq!(Color::from_hsva(211.0, 0.11, 0.59, 1.0), Color { r: 0x86, g: 0x8E, b: 0x96, a: 0xFF });
    assert_eq!(Color::from_hsva(299.0, 0.58, 0.91, 1.0), Color { r: 0xE6, g: 0x61, b: 0xE8, a: 0xFF });
    assert_eq!(Color::from_hsva(299.0 + 360.0, 0.58, 0.91, 1.0), Color { r: 0xE6, g: 0x61, b: 0xE8, a: 0xFF });
    assert_eq!(Color::from_hsva(299.0 - 360.0, 0.58, 0.91, 1.0), Color { r: 0xE6, g: 0x61, b: 0xE8, a: 0xFF });
    assert_eq!(Color::from_hsva(310.0, 0.33, 0.77, 1.0), Color { r: 0xC4, g: 0x84, b: 0xBA, a: 0xFF });
    assert_eq!(Color::from_hsva(310.0, 0.33, 0.77, 1.5), Color { r: 0xC4, g: 0x84, b: 0xBA, a: 0xFF });
    assert_eq!(Color::from_hsva(310.0, 0.33, 0.77, 0.5), Color { r: 0xC4, g: 0x84, b: 0xBA, a: 0x80 });
    assert_eq!(Color::from_hsva(310.0, 0.33, 0.77, 0.0), Color { r: 0xC4, g: 0x84, b: 0xBA, a: 0x00 });
    assert_eq!(Color::from_hsva(310.0, 0.33, 0.77, -0.2), Color { r: 0xC4, g: 0x84, b: 0xBA, a: 0x00 });
}
#[test]
fn test_color_rgb_to_hsv() {
    macro_rules! assert_close {
        ($c1:expr, $c2:expr) => {{
            let (h1, s1, v1, a1) = $c1;
            let (h2, s2, v2, a2) = $c2;
            let thresh = 1.0 / 255.0;
            assert!((h1 - h2).abs() < thresh, "{h1} vs {h2}");
            assert!((s1 - s2).abs() < thresh, "{s1} vs {s2}");
            assert!((v1 - v2).abs() < thresh, "{v1} vs {v2}");
            assert!((a1 - a2).abs() < thresh, "{a1} vs {a2}");
        }}
    }
    assert_close!(Color { r: 0x00, g: 0x00, b: 0x00, a: 0xFF }.to_hsva(), (0.0, 0.0, 0.0, 1.0));
    assert_close!(Color { r: 0x5C, g: 0x55, b: 0x55, a: 0xFF }.to_hsva(), (0.0, 0.076, 0.361, 1.0));
    assert_close!(Color { r: 92, g: 0, b: 0, a: 0xFF }.to_hsva(), (0.0, 1.0, 0.361, 1.0));
    assert_close!(Color { r: 92, g: 92, b: 92, a: 0xFF }.to_hsva(), (0.0, 0.0, 0.361, 1.0));
    assert_close!(Color { r: 0x40, g: 0x2D, b: 0x20, a: 0xFF }.to_hsva(), (24.375, 0.5, 0.251, 1.0));
    assert_close!(Color { r: 0x1F, g: 0x1A, b: 0x08, a: 0xFF }.to_hsva(), (46.956, 0.742, 0.122, 1.0));
    assert_close!(Color { r: 0xDC, g: 0xDE, b: 0xC3, a: 0xFF }.to_hsva(), (64.444, 0.122, 0.871, 1.0));
    assert_close!(Color { r: 252, g: 255, b: 224, a: 0xFF }.to_hsva(), (65.806, 0.122, 1.0, 1.0));
    assert_close!(Color { r: 0x7D, g: 0x8C, b: 0x6D, a: 0xFF }.to_hsva(), (89.032, 0.221, 0.549, 1.0));
    assert_close!(Color { r: 0xAC, g: 0xE8, b: 0xAC, a: 0xFF }.to_hsva(), (120.0, 0.259, 0.91, 1.0));
    assert_close!(Color { r: 0x00, g: 0x0A, b: 0x09, a: 0xFF }.to_hsva(), (174.0, 1.0, 0.039, 1.0));
    assert_close!(Color { r: 0x00, g: 0xFF, b: 0xFF, a: 0xFF }.to_hsva(), (180.0, 1.0, 1.0, 1.0));
    assert_close!(Color { r: 0x86, g: 0x8E, b: 0x96, a: 0xFF }.to_hsva(), (210.0, 0.107, 0.588, 1.0));
    assert_close!(Color { r: 0xE6, g: 0x61, b: 0xE8, a: 0xFF }.to_hsva(), (299.111, 0.582, 0.91, 1.0));
    assert_close!(Color { r: 0xC4, g: 0x84, b: 0xBA, a: 0xFF }.to_hsva(), (309.375, 0.327, 0.769, 1.0));
    assert_close!(Color { r: 0xC4, g: 0x84, b: 0xBA, a: 0x80 }.to_hsva(), (309.375, 0.327, 0.769, 0.5));
    assert_close!(Color { r: 0xC4, g: 0x84, b: 0xBA, a: 0x00 }.to_hsva(), (309.375, 0.327, 0.769, 0.0));
    assert_close!(Color { r: 255, g: 67, b: 14, a: 255 }.to_hsva(), (13.195, 0.945, 1.0, 1.0));
    assert_close!(Color { r: 255, g: 14, b: 67, a: 255 }.to_hsva(), (346.805, 0.945, 1.0, 1.0));
    assert_close!(Color { r: 87, g: 255, b: 33, a: 255 }.to_hsva(), (105.4054, 0.871, 1.0, 1.0));
    assert_close!(Color { r: 33, g: 255, b: 87, a: 255 }.to_hsva(), (134.594, 0.871, 1.0, 1.0));
    assert_close!(Color { r: 12, g: 54, b: 255, a: 255 }.to_hsva(), (229.629, 0.953, 1.0, 1.0));
    assert_close!(Color { r: 54, g: 12, b: 255, a: 255 }.to_hsva(), (250.37, 0.953, 1.0, 1.0));

    macro_rules! assert_round_trip {
        ($v:expr) => {{
            let rgba = $v;
            let hsva = rgba.to_hsva();
            let back = Color::from_hsva(hsva.0, hsva.1, hsva.2, hsva.3);
            assert_eq!(rgba, back);
        }}
    }
    assert_round_trip!(Color { r: 12, g: 65, b: 23, a: 87 });
    assert_round_trip!(Color { r: 128, g: 0, b: 23, a: 186 });
    assert_round_trip!(Color { r: 0, g: 0, b: 0, a: 0 });
    assert_round_trip!(Color { r: 0, g: 0, b: 0, a: 255 });
    assert_round_trip!(Color { r: 255, g: 0, b: 0, a: 255 });
    assert_round_trip!(Color { r: 0, g: 255, b: 0, a: 255 });
    assert_round_trip!(Color { r: 0, g: 0, b: 255, a: 255 });
    assert_round_trip!(Color { r: 255, g: 0, b: 0, a: 0 });
    assert_round_trip!(Color { r: 0, g: 255, b: 0, a: 0 });
    assert_round_trip!(Color { r: 0, g: 0, b: 255, a: 0 });
    assert_round_trip!(Color { r: 57, g: 0, b: 0, a: 0 });
    assert_round_trip!(Color { r: 0, g: 198, b: 0, a: 0 });
    assert_round_trip!(Color { r: 0, g: 0, b: 10, a: 0 });
}

#[derive(Debug, Clone, Copy, FromPrimitive)]
#[repr(u8)]
pub enum Property {
    XPos, YPos, Heading,

    Visible, Size,

    PenDown, PenSize, PenColor,
    PenColorH, PenColorS, PenColorV, PenColorT,

    Tempo, Volume, Balance,

    ColorH, ColorS, ColorV, ColorT,
    Fisheye, Whirl, Pixelate, Mosaic, Negative,
}
impl Property {
    pub(crate) fn from_effect(kind: &ast::EffectKind) -> Self {
        match kind {
            ast::EffectKind::Color => Property::ColorH,
            ast::EffectKind::Saturation => Property::ColorS,
            ast::EffectKind::Brightness => Property::ColorV,
            ast::EffectKind::Ghost => Property::ColorT,
            ast::EffectKind::Fisheye => Property::Fisheye,
            ast::EffectKind::Whirl => Property::Whirl,
            ast::EffectKind::Pixelate => Property::Pixelate,
            ast::EffectKind::Mosaic => Property::Mosaic,
            ast::EffectKind::Negative => Property::Negative,
        }
    }
    pub(crate) fn from_pen_attr(attr: &ast::PenAttribute) -> Self {
        match attr {
            ast::PenAttribute::Size => Property::PenSize,
            ast::PenAttribute::Hue => Property::PenColorH,
            ast::PenAttribute::Saturation => Property::PenColorS,
            ast::PenAttribute::Brightness => Property::PenColorV,
            ast::PenAttribute::Transparency => Property::PenColorT,
        }
    }
}

/// A collection of graphical effects related to an entity
#[derive(Clone, Copy)]
pub struct Effects {
    pub color_h: Number,
    pub color_s: Number,
    pub color_v: Number,
    pub color_t: Number,

    pub fisheye: Number,
    pub whirl: Number,
    pub pixelate: Number,
    pub mosaic: Number,
    pub negative: Number,
}
impl Default for Effects {
    fn default() -> Self {
        let zero = Number::new(0.0).unwrap();
        Self {
            color_h: zero,
            color_s: zero,
            color_v: zero,
            color_t: zero,

            fisheye: zero,
            whirl: zero,
            pixelate: zero,
            mosaic: zero,
            negative: zero,
        }
    }
}

/// A collection of properties related to an entity.
#[derive(Clone, Copy)]
pub struct Properties {
    pub pos: (Number, Number),
    pub heading: Number,

    pub visible: bool,
    pub size: Number,

    pub pen_down: bool,
    pub pen_size: Number,

    pub pen_color_h: Number,
    pub pen_color_s: Number,
    pub pen_color_v: Number,
    pub pen_color_t: Number,

    pub tempo: Number,
    pub volume: Number,
    pub balance: Number,

    pub effects: Effects,
}
impl Default for Properties {
    fn default() -> Self {
        let zero = Number::new(0.0).unwrap();
        let hundred = Number::new(100.0).unwrap();

        Self {
            pos: (zero, zero),
            heading: zero,

            visible: true,
            size: hundred,

            pen_down: false,
            pen_size: Number::new(1.0).unwrap(),

            pen_color_h: zero,
            pen_color_s: zero,
            pen_color_v: zero,
            pen_color_t: zero,

            tempo: Number::new(60.0).unwrap(),
            volume: hundred,
            balance: zero,

            effects: Default::default(),
        }
    }
}
impl Properties {
    fn with_value<C: CustomTypes<S>, S: System<C>, T>(&mut self, key: S::CommandKey, value: Result<T, ErrorCause<C, S>>, f: fn(&mut Self, T)) {
        match value {
            Ok(x) => {
                f(self, x);
                key.complete(Ok(()));
            }
            Err(e) => key.complete(Err(format!("{e:?}"))),
        }
    }

    pub fn perform_get_property<'gc, C: CustomTypes<S>, S: System<C>>(&self, key: S::RequestKey, prop: Property) -> RequestStatus<'gc, C, S> {
        let value: Json = match prop {
            Property::XPos => self.pos.0.get().into(),
            Property::YPos => self.pos.1.get().into(),
            Property::Heading => self.heading.get().into(),

            Property::Visible => self.visible.into(),
            Property::Size => self.size.get().into(),

            Property::PenDown => self.pen_down.into(),
            Property::PenSize => self.pen_size.get().into(),

            Property::PenColor => {
                let Color { a, r, g, b } = Color::from_hsva(
                    self.pen_color_h.get() as f32,
                    self.pen_color_s.get() as f32 / 100.0,
                    self.pen_color_v.get() as f32 / 100.0,
                    1.0 - self.pen_color_t.get() as f32 / 100.0
                );
                json!(u32::from_be_bytes([a, r, g, b]))
            }

            Property::PenColorH => self.pen_color_h.get().into(),
            Property::PenColorS => self.pen_color_s.get().into(),
            Property::PenColorV => self.pen_color_v.get().into(),
            Property::PenColorT => self.pen_color_t.get().into(),

            Property::Tempo => self.tempo.get().into(),
            Property::Volume => self.volume.get().into(),
            Property::Balance => self.balance.get().into(),

            Property::ColorH => self.effects.color_h.get().into(),
            Property::ColorS => self.effects.color_s.get().into(),
            Property::ColorV => self.effects.color_v.get().into(),
            Property::ColorT => self.effects.color_t.get().into(),

            Property::Fisheye => self.effects.fisheye.get().into(),
            Property::Whirl => self.effects.whirl.get().into(),
            Property::Pixelate => self.effects.pixelate.get().into(),
            Property::Mosaic => self.effects.mosaic.get().into(),
            Property::Negative => self.effects.negative.get().into(),
        };
        key.complete(Ok(C::Intermediate::from_json(value)));
        RequestStatus::Handled
    }
    pub fn perform_set_property<'gc, 'a, C: CustomTypes<S>, S: System<C>>(&mut self, key: S::CommandKey, prop: Property, value: Value<'gc, C, S>) -> CommandStatus<'gc, 'a, C, S> {
        match prop {
            Property::XPos => self.with_value(key, value.to_number().map_err(Into::into), |props, value| props.pos.0 = value),
            Property::YPos => self.with_value(key, value.to_number().map_err(Into::into), |props, value| props.pos.1 = value),
            Property::Heading => self.with_value(key, value.to_number().map_err(Into::into).and_then(|x| Number::new(x.get().rem_euclid(360.0)).map_err(Into::into)), |props, value| props.heading = value),

            Property::Visible => self.with_value(key, value.to_bool().map_err(Into::into), |props, value| props.visible = value),
            Property::Size => self.with_value(key, value.to_number().map_err(Into::into).and_then(|x| Number::new(x.get().max(0.0)).map_err(Into::into)), |props, value| props.size = value),

            Property::PenDown => self.with_value(key, value.to_bool().map_err(Into::into), |props, value| props.pen_down = value),
            Property::PenSize => self.with_value(key, value.to_number().map_err(Into::into).and_then(|x| Number::new(x.get().max(0.0)).map_err(Into::into)), |props, value| props.pen_size = value),

            Property::PenColor => self.with_value(key, value.to_number().map_err(Into::into), |props, value| {
                let [a, r, g, b] = (value.get() as u32).to_be_bytes();
                let (h, s, v, a) = Color { a, r, g, b }.to_hsva();
                props.pen_color_h = Number::new(h as f64).unwrap();
                props.pen_color_s = Number::new(s as f64 * 100.0).unwrap();
                props.pen_color_v = Number::new(v as f64 * 100.0).unwrap();
                props.pen_color_t = Number::new((1.0 - a as f64) * 100.0).unwrap();
            }),

            Property::PenColorH => self.with_value(key, value.to_number().map_err(Into::into).and_then(|x| Number::new(x.get().rem_euclid(360.0)).map_err(Into::into)), |props, value| props.pen_color_h = value),
            Property::PenColorS => self.with_value(key, value.to_number().map_err(Into::into).and_then(|x| Number::new(x.get().clamp(0.0, 100.0)).map_err(Into::into)), |props, value| props.pen_color_s = value),
            Property::PenColorV => self.with_value(key, value.to_number().map_err(Into::into).and_then(|x| Number::new(x.get().clamp(0.0, 100.0)).map_err(Into::into)), |props, value| props.pen_color_v = value),
            Property::PenColorT => self.with_value(key, value.to_number().map_err(Into::into).and_then(|x| Number::new(x.get().clamp(0.0, 100.0)).map_err(Into::into)), |props, value| props.pen_color_t = value),

            Property::Tempo => self.with_value(key, value.to_number().map_err(Into::into), |props, value| props.tempo = value),
            Property::Volume => self.with_value(key, value.to_number().map_err(Into::into), |props, value| props.volume = value),
            Property::Balance => self.with_value(key, value.to_number().map_err(Into::into), |props, value| props.balance = value),

            Property::ColorH => self.with_value(key, value.to_number().map_err(Into::into).and_then(|x| Number::new(x.get().rem_euclid(360.0)).map_err(Into::into)), |props, value| props.effects.color_h = value),
            Property::ColorS => self.with_value(key, value.to_number().map_err(Into::into).and_then(|x| Number::new(x.get().clamp(0.0, 100.0)).map_err(Into::into)), |props, value| props.effects.color_s = value),
            Property::ColorV => self.with_value(key, value.to_number().map_err(Into::into).and_then(|x| Number::new(x.get().clamp(0.0, 100.0)).map_err(Into::into)), |props, value| props.effects.color_v = value),
            Property::ColorT => self.with_value(key, value.to_number().map_err(Into::into).and_then(|x| Number::new(x.get().clamp(0.0, 100.0)).map_err(Into::into)), |props, value| props.effects.color_t = value),

            Property::Fisheye => self.with_value(key, value.to_number().map_err(Into::into), |props, value| props.effects.fisheye = value),
            Property::Whirl => self.with_value(key, value.to_number().map_err(Into::into), |props, value| props.effects.whirl = value),
            Property::Pixelate => self.with_value(key, value.to_number().map_err(Into::into), |props, value| props.effects.pixelate = value),
            Property::Mosaic => self.with_value(key, value.to_number().map_err(Into::into), |props, value| props.effects.mosaic = value),
            Property::Negative => self.with_value(key, value.to_number().map_err(Into::into), |props, value| props.effects.negative = value),
        }
        CommandStatus::Handled
    }
    pub fn perform_change_property<'gc, 'a, C: CustomTypes<S>, S: System<C>>(&mut self, key: S::CommandKey, prop: Property, delta: Value<'gc, C, S>) -> CommandStatus<'gc, 'a, C, S> {
        match prop {
            Property::XPos => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| self.pos.0.add(x).map_err(Into::into)), |props, value| props.pos.0 = value),
            Property::YPos => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| self.pos.1.add(x).map_err(Into::into)), |props, value| props.pos.1 = value),
            Property::Heading => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| Number::new((self.heading.get() + x.get()).rem_euclid(360.0)).map_err(Into::into)), |props, value| props.heading = value),

            Property::Visible => self.with_value(key, delta.to_bool().map_err(Into::into), |props, value| props.visible ^= value),
            Property::Size => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| Number::new((self.size.get() + x.get()).max(0.0)).map_err(Into::into)), |props, value| props.size = value),

            Property::PenDown => self.with_value(key, delta.to_bool().map_err(Into::into), |props, value| props.pen_down ^= value),
            Property::PenSize => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| Number::new((self.pen_size.get() + x.get()).max(0.0)).map_err(Into::into)), |props, value| props.pen_size = value),

            Property::PenColor => key.complete(Err("attempt to apply relative change to a color".into())),

            Property::PenColorH => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| Number::new((self.pen_color_h.get() + x.get()).rem_euclid(360.0)).map_err(Into::into)), |props, value| props.pen_color_h = value),
            Property::PenColorS => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| Number::new((self.pen_color_s.get() + x.get()).clamp(0.0, 100.0)).map_err(Into::into)), |props, value| props.pen_color_s = value),
            Property::PenColorV => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| Number::new((self.pen_color_v.get() + x.get()).clamp(0.0, 100.0)).map_err(Into::into)), |props, value| props.pen_color_v = value),
            Property::PenColorT => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| Number::new((self.pen_color_t.get() + x.get()).clamp(0.0, 100.0)).map_err(Into::into)), |props, value| props.pen_color_t = value),

            Property::Tempo => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| self.tempo.add(x).map_err(Into::into)), |props, value| props.tempo = value),
            Property::Volume => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| self.volume.add(x).map_err(Into::into)), |props, value| props.volume = value),
            Property::Balance => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| self.balance.add(x).map_err(Into::into)), |props, value| props.balance = value),

            Property::ColorH => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| Number::new((self.effects.color_h.get() + x.get()).rem_euclid(360.0)).map_err(Into::into)), |props, value| props.effects.color_h = value),
            Property::ColorS => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| Number::new((self.effects.color_s.get() + x.get()).clamp(0.0, 100.0)).map_err(Into::into)), |props, value| props.effects.color_s = value),
            Property::ColorV => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| Number::new((self.effects.color_v.get() + x.get()).clamp(0.0, 100.0)).map_err(Into::into)), |props, value| props.effects.color_v = value),
            Property::ColorT => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| Number::new((self.effects.color_t.get() + x.get()).clamp(0.0, 100.0)).map_err(Into::into)), |props, value| props.effects.color_t = value),

            Property::Fisheye => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| self.effects.fisheye.add(x).map_err(Into::into)), |props, value| props.effects.fisheye = value),
            Property::Whirl => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| self.effects.whirl.add(x).map_err(Into::into)), |props, value| props.effects.whirl = value),
            Property::Pixelate => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| self.effects.pixelate.add(x).map_err(Into::into)), |props, value| props.effects.pixelate = value),
            Property::Mosaic => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| self.effects.mosaic.add(x).map_err(Into::into)), |props, value| props.effects.mosaic = value),
            Property::Negative => self.with_value(key, delta.to_number().map_err(Into::into).and_then(|x| self.effects.negative.add(x).map_err(Into::into)), |props, value| props.effects.negative = value),
        }
        CommandStatus::Handled
    }

    pub fn perform_clear_effects<'gc, 'a, C: CustomTypes<S>, S: System<C>>(&mut self, key: S::CommandKey) -> CommandStatus<'gc, 'a, C, S> {
        self.effects = Default::default();
        key.complete(Ok(()));
        CommandStatus::Handled
    }

    pub fn perform_goto_xy<'gc, 'a, C: CustomTypes<S>, S: System<C>>(&mut self, key: S::CommandKey, x: Number, y: Number) -> CommandStatus<'gc, 'a, C, S> {
        self.pos = (x, y);
        key.complete(Ok(()));
        CommandStatus::Handled
    }

    pub fn perform_point_towards_xy<'gc, 'a, C: CustomTypes<S>, S: System<C>>(&mut self, key: S::CommandKey, x: Number, y: Number) -> CommandStatus<'gc, 'a, C, S> {
        let (dx, dy) = (x.get() - self.pos.0.get(), y.get() - self.pos.1.get());
        let heading = dx.atan2(dy).to_degrees().rem_euclid(360.0);
        self.with_value::<C, S, _>(key, Number::new(heading).map_err(Into::into), |props, value| props.heading = value);
        CommandStatus::Handled
    }

    pub fn perform_forward<'gc, 'a, C: CustomTypes<S>, S: System<C>>(&mut self, key: S::CommandKey, dist: Number) -> CommandStatus<'gc, 'a, C, S> {
        let (sin, cos) = self.heading.get().to_radians().sin_cos();
        let (x, y) = (self.pos.0.get() + sin * dist.get(), self.pos.1.get() + cos * dist.get());
        self.with_value::<C, S, _>(key, Number::new(x).map_err(Into::into).and_then(|x| Number::new(y).map(|y| (x, y)).map_err(Into::into)), |props, pos| props.pos = pos);
        CommandStatus::Handled
    }
}

/// A key from the keyboard.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    /// A normal character key, such as a letter, number, or special symbol.
    Char(char),
    /// The up arrow key.
    Up,
    /// The down arrow key.
    Down,
    /// The left arrow key.
    Left,
    /// The right arrow key.
    Right,
    /// Either enter/return key.
    Enter,
}

/// An event type which can be set to trigger the execution of a script.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone)]
pub enum Event {
    /// Fire when a green flag click event is issued.
    OnFlag,
    /// Fire when a new clone of this entity is created.
    OnClone,
    /// Fire when a message is received locally (Control message blocks). `None` is used to denote any message type.
    LocalMessage { msg_type: Option<String> },
    /// Fire when a message is received over the network (Network message blocks).
    NetworkMessage { msg_type: String, fields: Vec<String> },
    /// Fire when a key is pressed. [`None`] is used to denote any key press.
    OnKey { key_filter: Option<KeyCode> },
    /// Fire when explicitly requested from an input command.
    Custom { name: String, fields: Vec<String> },
}

#[derive(Debug, Clone, Copy, FromPrimitive)]
#[repr(u8)]
pub enum PrintStyle {
    Say, Think,
}

/// A value representing the identity of a [`Value`].
#[derive(Educe)]
#[educe(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
pub struct Identity<'gc, C: CustomTypes<S>, S: System<C>>(*const (), PhantomData<&'gc Value<'gc, C, S>>);

/// Gets the type of value that is stored.
pub trait GetType {
    type Output: Clone + Copy + PartialEq + Eq + fmt::Debug;
    /// Gets the type of value that is stored.
    fn get_type(&self) -> Self::Output;
}

pub trait Key<T> {
    fn complete(self, value: T);
}

/// Any primitive value.
#[derive(Educe, Collect)]
#[educe(Clone)]
#[collect(no_drop, bound = "")]
pub enum Value<'gc, C: CustomTypes<S>, S: System<C>> {
    /// A primitive boolean value.
    Bool(#[collect(require_static)] bool),
    /// A primitive numeric value. Snap! and NetsBlox use 64-bit floating point values for all numbers.
    Number(#[collect(require_static)] Number),
    /// A primitive string value, which is an immutable reference type.
    String(#[collect(require_static)] Rc<String>),
    /// An image stored as a binary buffer.
    Image(#[collect(require_static)] Rc<Vec<u8>>),
    /// An audio clip stored as as a binary buffer.
    Audio(#[collect(require_static)] Rc<Vec<u8>>),
    /// A reference to a native object handle produced by [`System`].
    Native(#[collect(require_static)] Rc<C::NativeValue>),
    /// A primitive list type, which is a mutable reference type.
    List(Gc<'gc, RefLock<VecDeque<Value<'gc, C, S>>>>),
    /// A closure/lambda function. This contains information about the closure's bytecode location, parameters, and captures from the parent scope.
    Closure(Gc<'gc, RefLock<Closure<'gc, C, S>>>),
    /// A reference to an [`Entity`] in the environment.
    Entity(Gc<'gc, RefLock<Entity<'gc, C, S>>>),
}

impl<'gc, C: CustomTypes<S>, S: System<C>> GetType for Value<'gc, C, S> {
    type Output = Type<C, S>;
    fn get_type(&self) -> Self::Output {
        match self {
            Value::Bool(_) => Type::Bool,
            Value::Number(_) => Type::Number,
            Value::String(_) => Type::String,
            Value::Image(_) => Type::Image,
            Value::Audio(_) => Type::Audio,
            Value::List(_) => Type::List,
            Value::Closure(_) => Type::Closure,
            Value::Entity(_) => Type::Entity,
            Value::Native(x) => Type::Native(x.get_type()),
        }
    }
}

#[derive(Clone, Copy)]
enum FormatStyle {
    Debug, Display,
}
impl<C: CustomTypes<S>, S: System<C>> fmt::Debug for Value<'_, C, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        format_value(self, f, FormatStyle::Debug)
    }
}
impl<C: CustomTypes<S>, S: System<C>> fmt::Display for Value<'_, C, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        format_value(self, f, FormatStyle::Display)
    }
}
fn format_value<C: CustomTypes<S>, S: System<C>>(value: &Value<'_, C, S>, f: &mut fmt::Formatter<'_>, style: FormatStyle) -> fmt::Result {
    fn print<'gc, C: CustomTypes<S>, S: System<C>>(value: &Value<'gc, C, S>, f: &mut fmt::Formatter<'_>, style: FormatStyle, cache: &mut BTreeSet<Identity<'gc, C, S>>) -> fmt::Result {
        match value {
            Value::Bool(x) => write!(f, "{x}"),
            Value::Number(x) => write!(f, "{x}"),
            Value::String(x) => match style {
                FormatStyle::Debug => write!(f, "{:?}", x.as_str()),
                FormatStyle::Display => write!(f, "{}", x.as_str()),
            }
            Value::Closure(x) => write!(f, "{:?}", &*x.borrow()),
            Value::Entity(x) => write!(f, "{:?}", &*x.borrow()),
            Value::Native(x) => write!(f, "{:?}", &**x),
            Value::Image(x) => write!(f, "[Image {:?}]", Rc::as_ptr(x)),
            Value::Audio(x) => write!(f, "[Audio {:?}]", Rc::as_ptr(x)),
            Value::List(x) => {
                let identity = value.identity();
                if !cache.insert(identity) { return write!(f, "[...]") }

                let x = x.borrow();
                write!(f, "[")?;
                for (i, val) in x.iter().enumerate() {
                    print(val, f, style, cache)?;
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
    let res = print(value, f, style, &mut cache);
    if res.is_ok() { debug_assert_eq!(cache.len(), 0); }
    res
}

impl<'gc, C: CustomTypes<S>, S: System<C>> From<bool> for Value<'gc, C, S> { fn from(v: bool) -> Self { Value::Bool(v) } }
impl<'gc, C: CustomTypes<S>, S: System<C>> From<Number> for Value<'gc, C, S> { fn from(v: Number) -> Self { Value::Number(v) } }
impl<'gc, C: CustomTypes<S>, S: System<C>> From<Rc<String>> for Value<'gc, C, S> { fn from(v: Rc<String>) -> Self { Value::String(v) } }
impl<'gc, C: CustomTypes<S>, S: System<C>> From<Gc<'gc, RefLock<VecDeque<Value<'gc, C, S>>>>> for Value<'gc, C, S> { fn from(v: Gc<'gc, RefLock<VecDeque<Value<'gc, C, S>>>>) -> Self { Value::List(v) } }
impl<'gc, C: CustomTypes<S>, S: System<C>> From<Gc<'gc, RefLock<Closure<'gc, C, S>>>> for Value<'gc, C, S> { fn from(v: Gc<'gc, RefLock<Closure<'gc, C, S>>>) -> Self { Value::Closure(v) } }
impl<'gc, C: CustomTypes<S>, S: System<C>> From<Gc<'gc, RefLock<Entity<'gc, C, S>>>> for Value<'gc, C, S> { fn from(v: Gc<'gc, RefLock<Entity<'gc, C, S>>>) -> Self { Value::Entity(v) } }
impl<'gc, C: CustomTypes<S>, S: System<C>> Value<'gc, C, S> {
    /// Create a new [`Value`] from a [`Json`] value.
    pub fn from_json(mc: &Mutation<'gc>, value: Json) -> Result<Self, FromJsonError> {
        Ok(match value {
            Json::Null => return Err(FromJsonError::HadNull),
            Json::Bool(x) => Value::Bool(x),
            Json::Number(x) => Value::Number(x.as_f64().and_then(|x| Number::new(x).ok()).ok_or(FromJsonError::HadBadNumber)?),
            Json::String(x) => Value::String(Rc::new(x)),
            Json::Array(x) => Value::List(Gc::new(mc, RefLock::new(x.into_iter().map(|x| Value::from_json(mc, x)).collect::<Result<_,_>>()?))),
            Json::Object(x) => Value::List(Gc::new(mc, RefLock::new(x.into_iter().map(|(k, v)| {
                let mut entry = VecDeque::with_capacity(2);
                entry.push_back(Value::String(Rc::new(k)));
                entry.push_back(Value::from_json(mc, v)?);
                Ok(Value::List(Gc::new(mc, RefLock::new(entry))))
            }).collect::<Result<_,_>>()?))),
        })
    }
    /// Converts a [`Value`] into [`Json`]. Note that not all values can be converted to json (e.g., cyclic lists or complex types).
    pub fn to_json(&self) -> Result<Json, ToJsonError<C, S>> {
        fn simplify<'gc, C: CustomTypes<S>, S: System<C>>(value: &Value<'gc, C, S>, cache: &mut BTreeSet<Identity<'gc, C, S>>) -> Result<Json, ToJsonError<C, S>> {
            Ok(match value {
                Value::Bool(x) => Json::Bool(*x),
                Value::Number(x) => Json::Number(JsonNumber::from_f64(x.get()).ok_or_else(|| ToJsonError::BadNumber(x.get()))?),
                Value::String(x) => Json::String(x.as_str().to_owned()),
                Value::Image(_) | Value::Audio(_) | Value::Closure(_) | Value::Entity(_) | Value::Native(_) => return Err(ToJsonError::ComplexType(value.get_type())),
                Value::List(x) => {
                    let identity = value.identity();
                    if !cache.insert(identity) { return Err(ToJsonError::Cyclic) }
                    let res = Json::Array(x.borrow().iter().map(|x| simplify(x, cache)).collect::<Result<_,_>>()?);
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
    pub fn identity(&self) -> Identity<'gc, C, S> {
        match self {
            Value::Bool(x) => Identity(x as *const bool as *const (), PhantomData),
            Value::Number(x) => Identity(x as *const Number as *const (), PhantomData),
            Value::String(x) => Identity(Rc::as_ptr(x) as *const (), PhantomData),
            Value::Image(x) => Identity(Rc::as_ptr(x) as *const (), PhantomData),
            Value::Audio(x) => Identity(Rc::as_ptr(x) as *const (), PhantomData),
            Value::List(x) => Identity(Gc::as_ptr(*x) as *const (), PhantomData),
            Value::Closure(x) => Identity(Gc::as_ptr(*x) as *const (), PhantomData),
            Value::Entity(x) => Identity(Gc::as_ptr(*x) as *const (), PhantomData),
            Value::Native(x) => Identity(Rc::as_ptr(x) as *const (), PhantomData),
        }
    }
    /// Attempts to interpret this value as a bool.
    pub fn to_bool(&self) -> Result<bool, ConversionError<C, S>> {
        Ok(match self {
            Value::Bool(x) => *x,
            x => return Err(ConversionError { got: x.get_type(), expected: Type::Bool }),
        })
    }
    /// Attempts to interpret this value as a number.
    pub fn to_number(&self) -> Result<Number, ConversionError<C, S>> {
        match self {
            Value::Number(x) => Ok(*x),
            Value::String(x) => {
                let parsed = match x.get(..2) {
                    Some("0x" | "0X") => i64::from_str_radix(&x[2..], 16).ok().map(|x| x as f64),
                    Some("0o" | "0O") => i64::from_str_radix(&x[2..], 8).ok().map(|x| x as f64),
                    Some("0b" | "0B") => i64::from_str_radix(&x[2..], 2).ok().map(|x| x as f64),
                    _ => x.parse::<f64>().ok(),
                };
                parsed.and_then(|x| Number::new(x).ok()).ok_or(ConversionError { got: Type::String, expected: Type::Number })
            }
            x => Err(ConversionError { got: x.get_type(), expected: Type::Number }),
        }
    }
    /// Attempts to interpret this value as a string.
    pub fn to_string(&self) -> Result<Cow<str>, ConversionError<C, S>> {
        Ok(match self {
            Value::String(x) => Cow::Borrowed(&**x),
            Value::Number(x) => Cow::Owned(x.to_string()),
            x => return Err(ConversionError { got: x.get_type(), expected: Type::String }),
        })
    }
    /// Attempts to interpret this value as an image.
    pub fn as_image(&self) -> Result<&Rc<Vec<u8>>, ConversionError<C, S>> {
        match self {
            Value::Image(x) => Ok(x),
            x => Err(ConversionError { got: x.get_type(), expected: Type::Image }),
        }
    }
    /// Attempts to interpret this value as a list.
    pub fn as_list(&self) -> Result<Gc<'gc, RefLock<VecDeque<Value<'gc, C, S>>>>, ConversionError<C, S>> {
        match self {
            Value::List(x) => Ok(*x),
            x => Err(ConversionError { got: x.get_type(), expected: Type::List }),
        }
    }
    /// Attempts to interpret this value as a closure.
    pub fn as_closure(&self) -> Result<Gc<'gc, RefLock<Closure<'gc, C, S>>>, ConversionError<C, S>> {
        match self {
            Value::Closure(x) => Ok(*x),
            x => Err(ConversionError { got: x.get_type(), expected: Type::Closure }),
        }
    }
    /// Attempts to interpret this value as an entity.
    pub fn as_entity(&self) -> Result<Gc<'gc, RefLock<Entity<'gc, C, S>>>, ConversionError<C, S>> {
        match self {
            Value::Entity(x) => Ok(*x),
            x => Err(ConversionError { got: x.get_type(), expected: Type::Entity }),
        }
    }
}

/// Information about a closure/lambda function.
#[derive(Collect)]
#[collect(no_drop, bound = "")]
pub struct Closure<'gc, C: CustomTypes<S>, S: System<C>> {
    #[collect(require_static)] pub pos: usize,
    #[collect(require_static)] pub params: Vec<String>,
                               pub captures: SymbolTable<'gc, C, S>,
}
impl<C: CustomTypes<S>, S: System<C>> fmt::Debug for Closure<'_, C, S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Closure {:#08x}", self.pos)
    }
}

/// The kind of entity being represented.
pub enum EntityKind<'gc, 'a, C: CustomTypes<S>, S: System<C>> {
    Stage { props: Properties },
    Sprite { props: Properties },
    Clone { parent: &'a Entity<'gc, C, S> },
}

/// Information about an entity (sprite or stage).
#[derive(Collect)]
#[collect(no_drop, bound = "")]
pub struct Entity<'gc, C: CustomTypes<S>, S: System<C>> {
    #[collect(require_static)] pub name: Rc<String>,
    #[collect(require_static)] pub costume_list: Rc<Vec<(String, Rc<Vec<u8>>)>>,
    #[collect(require_static)] pub costume: Option<Rc<Vec<u8>>>,
    #[collect(require_static)] pub state: C::EntityState,
    #[collect(require_static)] pub alive: bool,
                               pub root: Option<Gc<'gc, RefLock<Entity<'gc, C, S>>>>,
                               pub fields: SymbolTable<'gc, C, S>,
}
impl<C: CustomTypes<S>, S: System<C>> fmt::Debug for Entity<'_, C, S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Entity {:?}", self.name)
    }
}

/// Represents a variable whose value is being monitored.
/// 
/// Users can "show" or "hide" variables to create or remove watchers.
/// These can be used by students for a number of purposes, including debugging.
#[derive(Collect)]
#[collect(no_drop)]
pub struct Watcher<'gc, C: CustomTypes<S>, S: System<C>> {
    /// The entity associated with the variable being watched.
    pub entity: GcWeak<'gc, RefLock<Entity<'gc, C, S>>>,
    /// The name of the variable being watched.
    pub name: String,
    /// The value of the variable being watched.
    pub value: GcWeak<'gc, RefLock<Value<'gc, C, S>>>,
}
impl<'gc, C: CustomTypes<S>, S: System<C>> fmt::Debug for Watcher<'gc, C, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watcher").field("name", &self.name).finish_non_exhaustive()
    }
}

/// Represents a shared mutable resource.
/// 
/// This is effectively equivalent to [`Gc<T>`] except that it performs no dynamic allocation
/// for the [`Shared::Unique`] case, which is assumed to be significantly more likely than [`Shared::Aliased`].
#[derive(Collect)]
#[collect(no_drop)]
pub enum Shared<'gc, T: 'gc + Collect> {
    /// A shared resource which has only (this) single unique handle.
    Unique(T),
    /// One of several handles to a single shared resource.
    Aliased(Gc<'gc, RefLock<T>>),
}
impl<'gc, T: 'gc + Collect> Shared<'gc, T> {
    /// Sets the value of the shared resource.
    pub fn set(&mut self, mc: &Mutation<'gc>, value: T) {
        match self {
            Shared::Unique(x) => *x = value,
            Shared::Aliased(x) => *x.borrow_mut(mc) = value,
        }
    }
    /// Gets a reference to the shared resource's currently stored value.
    pub fn get(&self) -> SharedRef<T> {
        match self {
            Shared::Unique(x) => SharedRef::Unique(x),
            Shared::Aliased(x) => SharedRef::Aliased(x.borrow()),
        }
    }
    /// Transitions the shared value from [`Shared::Unique`] to [`Shared::Aliased`] if it has not already,
    /// and returns an additional alias to the underlying value.
    pub fn alias_inner(&mut self, mc: &Mutation<'gc>) -> Gc<'gc, RefLock<T>> {
        replace_with::replace_with(self, || unreachable!(), |myself| match myself {
            Shared::Unique(x) => Shared::Aliased(Gc::new(mc, RefLock::new(x))),
            Shared::Aliased(_) => myself,
        });

        match self {
            Shared::Unique(_) => unreachable!(),
            Shared::Aliased(x) => *x,
        }
    }
    /// Creates a new instance of [`Shared`] that references the same underlying value.
    /// This is equivalent to constructing an instance of [`Shared::Aliased`] with the result of [`Shared::alias_inner`].
    pub fn alias(&mut self, mc: &Mutation<'gc>) -> Self {
        Shared::Aliased(self.alias_inner(mc))
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
            SharedRef::Aliased(x) => x,
        }
    }
}

/// Holds a collection of variables in an execution context.
/// 
/// [`SymbolTable`] has utilities to extract variables from an abstract syntax tree, or to explicitly define variables.
/// Simple methods are provided to perform value lookups in the table.
#[derive(Collect, Educe)]
#[collect(no_drop, bound = "")]
#[educe(Default)]
pub struct SymbolTable<'gc, C: CustomTypes<S>, S: System<C>>(BTreeMap<String, Shared<'gc, Value<'gc, C, S>>>);
impl<'gc, C: CustomTypes<S>, S: System<C>> Clone for SymbolTable<'gc, C, S> {
    /// Creates a shallow (non-aliasing) copy of all variables currently stored in this symbol table.
    fn clone(&self) -> Self {
        let mut res = SymbolTable::default();
        for (k, v) in self.iter() {
            res.define_or_redefine(k, Shared::Unique(v.get().clone()));
        }
        res
    }
}
impl<'gc, C: CustomTypes<S>, S: System<C>> SymbolTable<'gc, C, S> {
    /// Defines or redefines a value in the symbol table to a new instance of [`Shared<Value>`].
    /// If a variable named `var` already existed and was [`Shared::Aliased`], its value is not modified.
    pub fn define_or_redefine(&mut self, var: &str, value: Shared<'gc, Value<'gc, C, S>>) {
        self.0.insert(var.to_owned(), value);
    }
    /// Defines a variable with an initial value if it does not already exist in this symbol table.
    /// If a variable with the given name already exists, this is a no-op.
    pub fn define_if_undefined<F: FnOnce() -> Shared<'gc, Value<'gc, C, S>>>(&mut self, var: &str, f: F) {
        if !self.0.contains_key(var) {
            self.0.insert(var.to_owned(), f());
        }
    }
    /// Looks up the given variable in the symbol table.
    /// If a variable with the given name does not exist, returns [`None`].
    pub fn lookup(&self, var: &str) -> Option<&Shared<'gc, Value<'gc, C, S>>> {
        self.0.get(var)
    }
    /// Equivalent to [`SymbolTable::lookup`] except that it returns a mutable reference.
    pub fn lookup_mut(&mut self, var: &str) -> Option<&mut Shared<'gc, Value<'gc, C, S>>> {
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
    pub fn iter(&self) -> symbol_table::Iter<'gc, '_, C, S> {
        symbol_table::Iter(self.0.iter())
    }
    /// Iterates over the key value pairs stored in the symbol table.
    pub fn iter_mut(&mut self) -> symbol_table::IterMut<'gc, '_, C, S> {
        symbol_table::IterMut(self.0.iter_mut())
    }
}
impl<'gc, C: CustomTypes<S>, S: System<C>> IntoIterator for SymbolTable<'gc, C, S> {
    type Item = (String, Shared<'gc, Value<'gc, C, S>>);
    type IntoIter = symbol_table::IntoIter<'gc, C, S>;
    fn into_iter(self) -> Self::IntoIter { symbol_table::IntoIter(self.0.into_iter()) }
}
impl<'gc, 'a, C: CustomTypes<S>, S: System<C>> IntoIterator for &'a SymbolTable<'gc, C, S> {
    type Item = <symbol_table::Iter<'gc, 'a, C, S> as Iterator>::Item;
    type IntoIter = symbol_table::Iter<'gc, 'a, C, S>;
    fn into_iter(self) -> Self::IntoIter { self.iter() }
}
impl<'gc, 'a, C: CustomTypes<S>, S: System<C>> IntoIterator for &'a mut SymbolTable<'gc, C, S> {
    type Item = <symbol_table::IterMut<'gc, 'a, C, S> as Iterator>::Item;
    type IntoIter = symbol_table::IterMut<'gc, 'a, C, S>;
    fn into_iter(self) -> Self::IntoIter { self.iter_mut() }
}
pub mod symbol_table {
    //! Special types for working with a [`SymbolTable`].
    use super::*;
    pub struct IntoIter<'gc, C: CustomTypes<S>, S: System<C>>(pub(crate) alloc::collections::btree_map::IntoIter<String, Shared<'gc, Value<'gc, C, S>>>);
    pub struct Iter<'gc, 'a, C: CustomTypes<S>, S: System<C>>(pub(crate) alloc::collections::btree_map::Iter<'a, String, Shared<'gc, Value<'gc, C, S>>>);
    pub struct IterMut<'gc, 'a, C: CustomTypes<S>, S: System<C>>(pub(crate) alloc::collections::btree_map::IterMut<'a, String, Shared<'gc, Value<'gc, C, S>>>);
    impl<'gc, C: CustomTypes<S>, S: System<C>> Iterator for IntoIter<'gc, C, S> { type Item = (String, Shared<'gc, Value<'gc, C, S>>); fn next(&mut self) -> Option<Self::Item> { self.0.next() } }
    impl<'gc, 'a, C: CustomTypes<S>, S: System<C>> Iterator for Iter<'gc, 'a, C, S> { type Item = (&'a String, &'a Shared<'gc, Value<'gc, C, S>>); fn next(&mut self) -> Option<Self::Item> { self.0.next() } }
    impl<'gc, 'a, C: CustomTypes<S>, S: System<C>> Iterator for IterMut<'gc, 'a, C, S> { type Item = (&'a String, &'a mut Shared<'gc, Value<'gc, C, S>>); fn next(&mut self) -> Option<Self::Item> { self.0.next() } }
}

/// A collection of symbol tables with hierarchical context searching.
pub(crate) struct LookupGroup<'gc, 'a, 'b, C: CustomTypes<S>, S: System<C>>(&'a mut [&'b mut SymbolTable<'gc, C, S>]);
impl<'gc, 'a, 'b, C: CustomTypes<S>, S: System<C>> LookupGroup<'gc, 'a, 'b, C, S> {
    /// Creates a new lookup group.
    /// The first symbol table is intended to be the most-global, and subsequent tables are increasingly more-local.
    pub fn new(tables: &'a mut [&'b mut SymbolTable<'gc, C, S>]) -> Self {
        debug_assert!(!tables.is_empty());
        Self(tables)
    }
    /// Searches for the given variable in this group of lookup tables,
    /// starting with the last (most-local) table and working towards the first (most-global) table.
    /// Returns a reference to the value if it is found, otherwise returns [`None`].
    pub fn lookup(&self, var: &str) -> Option<&Shared<'gc, Value<'gc, C, S>>> {
        for src in self.0.iter().rev() {
            if let Some(val) = src.lookup(var) {
                return Some(val);
            }
        }
        None
    }
    /// As [`LookupGroup::lookup`], but returns a mutable reference.
    pub fn lookup_mut(&mut self, var: &str) -> Option<&mut Shared<'gc, Value<'gc, C, S>>> {
        for src in self.0.iter_mut().rev() {
            if let Some(val) = src.lookup_mut(var) {
                return Some(val);
            }
        }
        None
    }
    /// Gets a mutable reference to the last (most-local) context.
    pub fn locals_mut(&mut self) -> &mut SymbolTable<'gc, C, S> {
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
pub struct GlobalContext<'gc, C: CustomTypes<S>, S: System<C>> {
    #[collect(require_static)] pub bytecode: Rc<ByteCode>,
    #[collect(require_static)] pub settings: Settings,
    #[collect(require_static)] pub system: Rc<S>,
    #[collect(require_static)] pub timer_start: u64,
    #[collect(require_static)] pub proj_name: String,
                               pub globals: SymbolTable<'gc, C, S>,
                               pub entities: BTreeMap<String, Gc<'gc, RefLock<Entity<'gc, C, S>>>>,
}
impl<'gc, C: CustomTypes<S>, S: System<C>> GlobalContext<'gc, C, S> {
    pub fn from_init(mc: &Mutation<'gc>, init_info: &InitInfo, bytecode: Rc<ByteCode>, settings: Settings, system: Rc<S>) -> Self {
        let allocated_refs = init_info.ref_values.iter().map(|ref_value| match ref_value {
            RefValue::String(value) => Value::String(Rc::new(value.clone())),
            RefValue::Image(content) => Value::Image(Rc::new(content.clone())),
            RefValue::List(_) => Value::List(Gc::new(mc, Default::default())),
        }).collect::<Vec<_>>();

        fn get_value<'gc, C: CustomTypes<S>, S: System<C>>(value: &InitValue, allocated_refs: &[Value<'gc, C, S>]) -> Value<'gc, C, S> {
            match value {
                InitValue::Bool(x) => Value::Bool(*x),
                InitValue::Number(x) => Value::Number(*x),
                InitValue::Ref(x) => allocated_refs[*x].clone(),
            }
        }

        for (allocated_ref, ref_value) in iter::zip(&allocated_refs, &init_info.ref_values) {
            match ref_value {
                RefValue::String(_) | RefValue::Image(_) => continue, // we already populated these values in the first pass
                RefValue::List(values) => {
                    let allocated_ref = match allocated_ref {
                        Value::List(x) => x,
                        _ => unreachable!(),
                    };
                    let mut allocated_ref = allocated_ref.borrow_mut(mc);
                    for value in values {
                        allocated_ref.push_back(get_value(value, &allocated_refs));
                    }
                }
            }
        }

        let mut globals = SymbolTable::default();
        for (global, value) in init_info.globals.iter() {
            globals.define_or_redefine(global, Shared::Unique(get_value(value, &allocated_refs)));
        }

        let mut entities = BTreeMap::new();
        for (i, entity_info) in init_info.entities.iter().enumerate() {
            let mut fields = SymbolTable::default();
            for (field, value) in entity_info.fields.iter() {
                fields.define_or_redefine(field, Shared::Unique(get_value(value, &allocated_refs)));
            }

            let costume_list = {
                let mut res = Vec::with_capacity(entity_info.costumes.len());
                for (name, value) in entity_info.costumes.iter() {
                    let image = match get_value(value, &allocated_refs) {
                        Value::Image(x) => x.clone(),
                        _ => unreachable!(),
                    };
                    res.push((name.clone(), image));
                }
                Rc::new(res)
            };

            let costume = entity_info.active_costume.and_then(|x| costume_list.get(x)).map(|x| x.1.clone());

            let mut props = Properties::default();
            props.visible = entity_info.visible;
            props.size = entity_info.size;
            props.pos = entity_info.pos;
            props.heading = entity_info.heading;

            let (r, g, b, a) = entity_info.color;
            let (h, s, v, a) = Color { r, g, b, a }.to_hsva();
            props.pen_color_h = Number::new(h as f64).unwrap();
            props.pen_color_s = Number::new(s as f64 * 100.0).unwrap();
            props.pen_color_v = Number::new(v as f64 * 100.0).unwrap();
            props.pen_color_t = Number::new((1.0 - a as f64) * 100.0).unwrap();

            let kind = if i == 0 { EntityKind::Stage { props } } else { EntityKind::Sprite { props } };
            let name = Rc::new(entity_info.name.clone());
            let state = kind.into();

            entities.insert(entity_info.name.clone(), Gc::new(mc, RefLock::new(Entity { alive: true, root: None, name, fields, costume_list, costume, state })));
        }

        let proj_name = init_info.proj_name.clone();
        let timer_start = system.time().to_arbitrary_ms::<C, S>().unwrap_or(0);

        Self { proj_name, globals, entities, timer_start, system, settings, bytecode }
    }
}

pub enum OutgoingMessage<C: CustomTypes<S>, S: System<C>> {
    Normal {
        msg_type: String,
        values: Vec<(String, Json)>,
        targets: Vec<String>,
    },
    Blocking {
        msg_type: String,
        values: Vec<(String, Json)>,
        targets: Vec<String>,
        reply_key: S::ExternReplyKey,
    },
    Reply {
        value: Json,
        reply_key: S::InternReplyKey,
    },
}
pub struct IncomingMessage<C: CustomTypes<S>, S: System<C>> {
    pub msg_type: String,
    pub values: Vec<(String, Json)>,
    pub reply_key: Option<S::InternReplyKey>,
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
    /// which is expected to be usable to later obtain an [`AsyncResult<T>`].
    Async(K),
}

/// The result of a successful call to an async poller operation such as in [`System`].
pub enum AsyncResult<T> {
    /// The async operation is still pending and has not completed.
    Pending,
    /// The async operation completed with the given value.
    Completed(T),
    /// The async operation was completed and the result was already consumed.
    Consumed,
}
impl<T> AsyncResult<T> {
    /// Constructs a new async result handle in the [`AsyncResult::Pending`] state.
    pub fn new() -> Self {
        Self::Pending
    }
    /// Transitions from the [`AsyncResult::Pending`] state to [`AsyncResult::Completed`] with the provided result value.
    /// If this async result handle has already been completed, [`Err`] is returned with the passed value.
    pub fn complete(&mut self, value: T) -> Result<(), T> {
        match self {
            AsyncResult::Pending => {
                *self = AsyncResult::Completed(value);
                Ok(())
            }
            AsyncResult::Completed(_) | AsyncResult::Consumed => Err(value),
        }
    }
    /// Polls the status of the async operation.
    /// A [`AsyncResult::Completed`] result transitions permanently to the [`AsyncResult::Consumed`] state.
    pub fn poll(&mut self) -> Self {
        match self {
            AsyncResult::Pending => AsyncResult::Pending,
            AsyncResult::Completed(_) | AsyncResult::Consumed => mem::replace(self, AsyncResult::Consumed),
        }
    }
}

/// Types of [`System`] resources, grouped into feature categories.
#[derive(Debug)]
pub enum Feature {
    /// The ability of a process to get the current time with respect to an arbitrary starting point.
    ArbitraryTime,
    /// The ability of a process to get the current real time.
    RealTime,

    /// The ability of a process to request keyboard input from the user.
    Input,
    /// The ability of a process to display information.
    Print,

    /// The ability of a process to perform a syscall of the given name.
    Syscall { name: String },
    /// The ability of a process to perform an RPC call.
    Rpc { service: String, rpc: String },

    /// The ability of an entity to get a certain property.
    GetProperty { prop: Property },
    /// The ability of an entity to set a certain property.
    SetProperty { prop: Property },
    /// The ability of an entity to apply a relative change to a certain property.
    ChangeProperty { prop: Property },

    /// The ability of an entity to change the current costume.
    SetCostume,

    /// The ability to clear all graphic effects on an entity. This is equivalent to setting all the graphic effect properties to zero.
    ClearEffects,
    /// The ability to clear all drawings made by all sprites.
    ClearDrawings,

    /// The ability of an entity to set both its x and y positions simultaneously.
    GotoXY,
    /// The ability of an entity to go the the same location as another entity.
    GotoEntity,

    /// The ability of an entity to turn to face a specific location.
    PointTowardsXY,
    /// The ability of an entity to turn to face another entity.
    PointTowardsEntity,

    /// The ability of an entity to move forward or backwards by a distance.
    Forward,

    /// The ability of an entity to execute a specific block that was not built in to the ast parser or bytecode compiler (e.g., extension blocks).
    UnknownBlock { name: String },
}

/// A value-returning request issued from the runtime.
pub enum Request<'gc, C: CustomTypes<S>, S: System<C>> {
    /// Request input from the user. The `prompt` argument is either [`Some`] prompt to display, or [`None`] for no prompt.
    Input { prompt: Option<Value<'gc, C, S>> },
    /// Performs a system call on the local hardware to access device resources.
    Syscall { name: String, args: Vec<Value<'gc, C, S>> },
    /// Requests the system to execute the given RPC.
    Rpc { service: String, rpc: String, args: Vec<(String, Value<'gc, C, S>)> },
    /// Request to get the current value of an entity property.
    Property { prop: Property },
    /// Request to run a block which was not known by the ast parser or bytecode compiler.
    /// This is typically used for implementing extension blocks in the VM, which cannot be handled otherwise.
    UnknownBlock { name: String, args: Vec<Value<'gc, C, S>> },
}
impl<'gc, C: CustomTypes<S>, S: System<C>> Request<'gc, C, S> {
    /// Gets the [`Feature`] associated with this request.
    pub fn feature(&self) -> Feature {
        match self {
            Request::Input { .. } => Feature::Input,
            Request::Syscall { name, .. } => Feature::Syscall { name: name.clone() },
            Request::Rpc { service, rpc, .. } => Feature::Rpc { service: service.clone(), rpc: rpc.clone() },
            Request::Property { prop } => Feature::GetProperty { prop: *prop },
            Request::UnknownBlock { name, .. } => Feature::UnknownBlock { name: name.clone() },
        }
    }
}

/// A non-value-returning command issued from the runtime.
pub enum Command<'gc, 'a, C: CustomTypes<S>, S: System<C>> {
    /// Output [`Some`] [`Value`] or [`None`] to perform a Snap!-style clear.
    Print { style: PrintStyle, value: Option<Value<'gc, C, S>> },

    /// Set an entity property to a specific value.
    SetProperty { prop: Property, value: Value<'gc, C, S> },
    /// Apply a relative change to the value of an entity property.
    ChangeProperty { prop: Property, delta: Value<'gc, C, S> },

    /// Clear all graphic effects on the entity. This is equivalent to setting all the graphic effect properties to zero.
    ClearEffects,
    /// Clear all drawings made by all sprites.
    ClearDrawings,

    /// Sets the costume on the entity. This should essentially assigns the costume to [`Entity::costume`],
    /// but is treated as a system command so that custom code can be executed when an entity switches costumes.
    SetCostume { costume: Option<Rc<Vec<u8>>> },

    /// Moves the entity to a specific location.
    GotoXY { x: Number, y: Number },
    /// Moves the current entity to the same position as the target entity.
    GotoEntity { target: &'a Entity<'gc, C, S> },

    /// Points the entity towards a specific location.
    PointTowardsXY { x: Number, y: Number },
    /// Points the current entity towards a target entity.
    PointTowardsEntity { target: &'a Entity<'gc, C, S> },

    /// Move forward by a given distance. If the distance is negative, move backwards instead.
    Forward { distance: Number },
}
impl<'gc, C: CustomTypes<S>, S: System<C>> Command<'gc, '_, C, S> {
    /// Gets the [`Feature`] associated with this command.
    pub fn feature(&self) -> Feature {
        match self {
            Command::Print { .. } => Feature::Print,
            Command::SetProperty { prop, .. } => Feature::SetProperty { prop: *prop },
            Command::ChangeProperty { prop, .. } => Feature::ChangeProperty { prop: *prop },
            Command::SetCostume { .. } => Feature::SetCostume,
            Command::ClearEffects { .. } => Feature::ClearEffects,
            Command::ClearDrawings => Feature::ClearDrawings,
            Command::GotoXY { .. } => Feature::GotoXY,
            Command::GotoEntity { .. } => Feature::GotoEntity,
            Command::PointTowardsXY { .. } => Feature::PointTowardsXY,
            Command::PointTowardsEntity { .. } => Feature::PointTowardsEntity,
            Command::Forward { .. } => Feature::Forward,
        }
    }
}

/// The status of a potentially-handled request.
pub enum RequestStatus<'gc, C: CustomTypes<S>, S: System<C>> {
    /// The request was handled by the overriding client.
    Handled,
    /// The request was not handled by the overriding client,
    /// and the default system implementation should be used instead.
    UseDefault { key: S::RequestKey, request: Request<'gc, C, S> },
}
/// The status of a potentially-handled command.
pub enum CommandStatus<'gc, 'a, C: CustomTypes<S>, S: System<C>> {
    /// The command was handled by the overriding client.
    Handled,
    /// The command was not handled by the overriding client,
    /// and the default system implementation should be used instead.
    UseDefault { key: S::CommandKey, command: Command<'gc, 'a, C, S> },
}

/// A collection of implementation options that could be used for implementing a customizable [`System`].
#[derive(Educe)]
#[educe(Clone)]
pub struct Config<C: CustomTypes<S>, S: System<C>> {
    /// A function used to perform asynchronous requests that yield a value back to the runtime.
    pub request: Option<Rc<dyn for<'gc> Fn(&S, &Mutation<'gc>, S::RequestKey, Request<'gc, C, S>, &mut Entity<'gc, C, S>) -> RequestStatus<'gc, C, S>>>,
    /// A function used to perform asynchronous tasks whose completion is awaited by the runtime.
    pub command: Option<Rc<dyn for<'gc, 'a> Fn(&S, &Mutation<'gc>, S::CommandKey, Command<'gc, 'a, C, S>, &mut Entity<'gc, C, S>) -> CommandStatus<'gc, 'a, C, S>>>,
}
impl<C: CustomTypes<S>, S: System<C>> Default for Config<C, S> {
    fn default() -> Self {
        Self {
            request: None,
            command: Some(Rc::new(|_, _, key, command, entity| match command {
                Command::SetCostume { costume } => {
                    entity.costume = costume;
                    key.complete(Ok(()));
                    CommandStatus::Handled
                }
                _ => CommandStatus::UseDefault { key, command },
            })),
        }
    }
}
impl<C: CustomTypes<S>, S: System<C>> Config<C, S> {
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

/// The required interface used by [`CustomTypes::Intermediate`].
pub trait IntermediateType {
    fn from_json(json: Json) -> Self;
    fn from_image(img: Vec<u8>) -> Self;
    fn from_audio(audio: Vec<u8>) -> Self;
}

/// A collection of static settings for using custom native types.
pub trait CustomTypes<S: System<Self>>: 'static + Sized {
    /// A native type that can be exposed directly to the VM as a value of type [`Value::Native`].
    /// This could, for example, be used to allow a process to hold on to a file handle stored in a variable.
    /// For type checking, this is required to implement [`GetType`], which can use a custom type enum.
    /// Native types have reference semantics in the vm.
    type NativeValue: 'static + GetType + fmt::Debug;

    /// An intermediate type used to produce a [`Value`] for the [`System`] under async context.
    /// The reason this is needed is that [`Value`] can only be used during the lifetime of an associated [`Mutation`] handle,
    /// which cannot be extended into the larger lifetime required for async operations.
    /// Conversions are automatically performed from this type to [`Value`] via [`CustomTypes::from_intermediate`].
    type Intermediate: 'static + Send + IntermediateType;

    /// Type used to represent an entity's system-specific state.
    /// This should include any details outside of core process functionality (e.g., graphics, position, orientation).
    /// This type should be constructable from [`EntityKind`], which is used to initialize a new entity in the runtime.
    type EntityState: 'static + for<'gc, 'a> From<EntityKind<'gc, 'a, Self, S>>;

    /// Converts a [`Value`] into a [`CustomTypes::Intermediate`] for use outside of gc context.
    fn from_intermediate<'gc>(mc: &Mutation<'gc>, value: Self::Intermediate) -> Result<Value<'gc, Self, S>, ErrorCause<Self, S>>;
}

/// The time as determined by an implementation of [`System`].
pub enum SysTime {
    /// No concept of time. This should only be produced as a last resort, as it disables all time-based features.
    Timeless,
    /// A time measurement from an arbitrary (but consistent during runtime) starting point, which must be measured in milliseconds.
    /// For instance, this could be used to measure uptime on systems that do not support reading real time.
    Arbitrary { ms: u64 },
    /// A real-world time measurement.
    /// This is always preferable over [`SysTime::Arbitrary`].
    /// The value is assumed to already be transformed to local time.
    Real { local: OffsetDateTime },
}
impl SysTime {
    /// Attempt to convert this time into milliseconds after some arbitrary starting point.
    pub fn to_arbitrary_ms<C: CustomTypes<S>, S: System<C>>(&self) -> Result<u64, ErrorCause<C, S>> {
        match self {
            Self::Timeless => Err(ErrorCause::NotSupported { feature: Feature::ArbitraryTime }),
            Self::Arbitrary { ms } => Ok(*ms),
            Self::Real { local } => Ok((local.unix_timestamp_nanos() / 1000000) as u64),
        }
    }
    /// Attempt to convert this time into a real world time in the local timezone.
    pub fn to_real_local<C: CustomTypes<S>, S: System<C>>(&self) -> Result<time::OffsetDateTime, ErrorCause<C, S>> {
        match self {
            Self::Timeless | Self::Arbitrary { .. } => Err(ErrorCause::NotSupported { feature: Feature::RealTime }),
            Self::Real { local } => Ok(*local),
        }
    }
}

/// Represents all the features of an implementing system.
/// 
/// This type encodes any features that cannot be performed without platform-specific resources.
/// 
/// When implementing [`System`] for some type, you may prefer to not support one or more features.
/// This can be accomplished by returning the [`ErrorCause::NotSupported`] variant for the relevant [`Feature`].
pub trait System<C: CustomTypes<Self>>: 'static + Sized {
    /// Key type used to await the result of an asynchronous request.
    type RequestKey: 'static + Key<Result<C::Intermediate, String>>;
    /// Key type used to await the completion of an asynchronous command.
    type CommandKey: 'static + Key<Result<(), String>>;

    /// Key type used to await the result of a "send message and wait" block (response from target).
    type ExternReplyKey: 'static;
    /// Key type used to reply to a message that was sent to this client with the expectation of receiving a response.
    /// This type is required to be [`Clone`] because there can be multiple message handlers for the same message type.
    type InternReplyKey: 'static + Clone;

    /// Gets a random value sampled from the given `range`, which is assumed to be non-empty.
    /// The input for this generic function is such that it is compatible with [`rand::Rng::gen_range`],
    /// which makes it possible to implement this function with any random provider under the [`rand`] crate standard.
    fn rand<T: SampleUniform, R: SampleRange<T>>(&self, range: R) -> T;

    /// Gets the current system time.
    fn time(&self) -> SysTime;

    /// Performs a general request which returns a value to the system.
    /// Ideally, this function should be non-blocking, and the requestor will await the result asynchronously.
    /// The [`Entity`] that made the request is provided for context.
    fn perform_request<'gc>(&self, mc: &Mutation<'gc>, request: Request<'gc, C, Self>, entity: &mut Entity<'gc, C, Self>) -> Result<MaybeAsync<Result<Value<'gc, C, Self>, String>, Self::RequestKey>, ErrorCause<C, Self>>;
    /// Poll for the completion of an asynchronous request.
    /// The [`Entity`] that made the request is provided for context.
    fn poll_request<'gc>(&self, mc: &Mutation<'gc>, key: &Self::RequestKey, entity: &mut Entity<'gc, C, Self>) -> Result<AsyncResult<Result<Value<'gc, C, Self>, String>>, ErrorCause<C, Self>>;

    /// Performs a general command which does not return a value to the system.
    /// Ideally, this function should be non-blocking, and the commander will await the task's completion asynchronously.
    /// The [`Entity`] that issued the command is provided for context.
    fn perform_command<'gc>(&self, mc: &Mutation<'gc>, command: Command<'gc, '_, C, Self>, entity: &mut Entity<'gc, C, Self>) -> Result<MaybeAsync<Result<(), String>, Self::CommandKey>, ErrorCause<C, Self>>;
    /// Poll for the completion of an asynchronous command.
    /// The [`Entity`] that issued the command is provided for context.
    fn poll_command<'gc>(&self, mc: &Mutation<'gc>, key: &Self::CommandKey, entity: &mut Entity<'gc, C, Self>) -> Result<AsyncResult<Result<(), String>>, ErrorCause<C, Self>>;

    /// Sends a message containing a set of named `values` to each of the specified `targets`.
    /// The `expect_reply` value controls whether or not to use a reply mechanism to asynchronously receive a response from the target(s).
    /// In the case that there are multiple targets, only the first reply (if any) should be used.
    fn send_message(&self, msg_type: String, values: Vec<(String, Json)>, targets: Vec<String>, expect_reply: bool) -> Result<Option<Self::ExternReplyKey>, ErrorCause<C, Self>>;
    /// Polls for a response from a client initiated by [`System::send_message`].
    /// If the client responds, a value of [`Some(x)`] is returned.
    /// The system may elect to impose a timeout for reply results, in which case [`None`] is returned instead.
    fn poll_reply(&self, key: &Self::ExternReplyKey) -> AsyncResult<Option<Json>>;
    /// Sends a reply to the sender of a blocking message this client received.
    fn send_reply(&self, key: Self::InternReplyKey, value: Json) -> Result<(), ErrorCause<C, Self>>;
    /// Attempts to receive a message from the message buffer.
    /// This operation is always non-blocking and returns [`None`] if there are no messages in the buffer.
    /// If a message is received, a tuple of form `(msg_type, values, reply_key)` is returned.
    fn receive_message(&self) -> Option<IncomingMessage<C, Self>>;
}
