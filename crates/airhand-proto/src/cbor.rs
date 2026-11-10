//! CBOR 编码的辅助：哪些地方、怎样容忍对方版本里新增的东西（见 [`crate::framing`] 的"兼容规则"）。
//!
//! # 两种枚举
//!
//! * **只有编号的枚举**（编码格式、平台、结束原因、握手模式……）一律是"开放枚举"（`open_enum!`，见本模块）：线上就是一个整数，
//!   不认识的值解码成 `Unknown(原值)`，**从不出错**。收的一方自己决定怎么对待它（跳过、当作某个默认值、
//!   当成"对方的版本更新"），日志里还看得到原值。所以它们放在哪里都行：字段、列表、`Option` 里；
//! * **带字段的枚举**（消息、提示、输入事件）不认识的变体没法表示，只能整个跳过。只有三个地方允许跳过，而且都要写明：
//!   顶层的消息（[`crate::framing::decode`]）、[`opt`]（`Option<枚举>` 字段）、[`open_vec`]（列表里的元素）。
//!   别的地方遇到不认识的变体是**解码错误**，不会悄悄跳过别的东西。
//!
//! # 为什么跳过要这么严格
//!
//! minicbor 派生的代码对 `Option<T>` 字段自带一个"容忍"：解 `T` 时遇到**任何深度**的不认识的变体，就把这个字段当成
//! `None`，再从出错的位置跳过一项。可出错的位置在 `T` 的中间，跳过一项之后后面的字段就都错位了（`T` 后面还有
//! 两个以上的字段时，解出来的是错的值，或者报一个莫名其妙的错）。所以：
//!
//! * 带字段的枚举不能出现在派生的 `Option<…>` 里（包括放在结构体里、结构体再放进 `Option`），要用 [`opt`]；
//! * 这里的跳过只认"这一项**本身**的变体不认识"（[`unknown_here`]）。更深处的不认识换成普通的解码错误（[`strict`]）：
//!   外层派生的 `Option` 处理只认"不认识的变体"这种错误，换掉之后它就没法再把错误吞掉、错位跳过。
//!
//! 只有编号的枚举都是开放的，不会产生"不认识的变体"，所以上面的问题只可能来自带字段的枚举，而它们只有那三个地方能放。

use minicbor::data::Type;
use minicbor::decode::{Decoder, Error};
use minicbor::encode::{Encoder, Write};
use minicbor::{Decode, Encode};

/// `item`（一个完整的 CBOR 值）的变体编号在哪里：带字段的枚举是 `[编号, 字段]`，编号在数组头后面；
/// 只有编号的枚举就是开头
fn variant_at(item: &[u8]) -> usize {
    let mut d = Decoder::new(item);
    match d.datatype() {
        Ok(Type::Array | Type::ArrayIndef) => d.array().map_or(0, |_| d.position()),
        _ => 0,
    }
}

/// `item` 的变体编号（日志用）；不是枚举时为 `None`
pub fn variant_of(item: &[u8]) -> Option<i64> {
    let mut d = Decoder::new(item);
    d.set_position(variant_at(item));
    d.i64().ok()
}

/// 解码 `item` 遇到的错误是不是"`item` 本身的变体不认识"（不是里面更深处的）
pub fn unknown_here(e: &Error, item: &[u8]) -> bool {
    e.is_unknown_variant() && e.position() == Some(variant_at(item))
}

/// 把"不认识的变体"换成普通的解码错误（见模块说明）。别的错误原样返回
pub fn strict(e: Error) -> Error {
    if !e.is_unknown_variant() {
        return e;
    }
    let pos = e.position();
    let e = Error::message(format!(
        "{e}: an unknown variant where the protocol doesn't allow skipping (see airhand_proto::cbor)"
    ));
    match pos {
        Some(p) => e.at(p),
        None => e,
    }
}

/// 解码 `d` 接下来的一个值；这个值本身是不认识的变体时返回 `Ok(None)`（整个跳过），其它错误照常返回
fn tolerant<'b, Ctx, T: Decode<'b, Ctx>>(
    d: &mut Decoder<'b>,
    ctx: &mut Ctx,
) -> Result<Option<T>, Error> {
    let input = d.input();
    let start = d.position();
    d.skip()?;
    let item = &input[start..d.position()];
    match T::decode(&mut Decoder::new(item), ctx) {
        Ok(v) => Ok(Some(v)),
        Err(e) if unknown_here(&e, item) => {
            tracing::debug!(variant = ?variant_of(item), "skipping an item from a newer version");
            Ok(None)
        }
        Err(e) => Err(strict(e)),
    }
}

/// 列表里不认识的元素（对方版本新增的带字段的枚举变体，例如新的输入事件）跳过，其余照收。
/// 用法：`#[cbor(n(i), with = "crate::cbor::open_vec")]`
pub mod open_vec {
    use super::*;

    pub fn encode<Ctx, T: Encode<Ctx>, W: Write>(
        v: &[T],
        e: &mut Encoder<W>,
        ctx: &mut Ctx,
    ) -> Result<(), minicbor::encode::Error<W::Error>> {
        e.array(v.len() as u64)?;
        for x in v {
            x.encode(e, ctx)?;
        }
        Ok(())
    }

    pub fn decode<'b, Ctx, T: Decode<'b, Ctx>>(
        d: &mut Decoder<'b>,
        ctx: &mut Ctx,
    ) -> Result<Vec<T>, Error> {
        let mut out = Vec::new();
        match d.array()? {
            Some(n) => {
                for _ in 0..n {
                    out.extend(tolerant(d, ctx)?);
                }
            }
            None => loop {
                if d.datatype()? == Type::Break {
                    d.skip()?;
                    break;
                }
                out.extend(tolerant(d, ctx)?);
            },
        }
        Ok(out)
    }
}

/// `Option<带字段的枚举>`：没有值（null）、或者不认识的变体（整个跳过）都是 `None`（见模块说明）。
/// 用法：`#[cbor(n(i), with = "crate::cbor::opt", has_nil)]`
pub mod opt {
    use super::*;

    pub fn encode<Ctx, T: Encode<Ctx>, W: Write>(
        v: &Option<T>,
        e: &mut Encoder<W>,
        ctx: &mut Ctx,
    ) -> Result<(), minicbor::encode::Error<W::Error>> {
        match v {
            Some(x) => x.encode(e, ctx),
            None => {
                e.null()?;
                Ok(())
            }
        }
    }

    pub fn decode<'b, Ctx, T: Decode<'b, Ctx>>(
        d: &mut Decoder<'b>,
        ctx: &mut Ctx,
    ) -> Result<Option<T>, Error> {
        if matches!(d.datatype()?, Type::Null | Type::Undefined) {
            d.skip()?;
            return Ok(None);
        }
        tolerant(d, ctx)
    }

    pub fn nil<T>() -> Option<Option<T>> {
        Some(None)
    }

    pub fn is_nil<T>(v: &Option<T>) -> bool {
        v.is_none()
    }
}

/// 开放枚举（见模块说明）：只有编号的枚举，线上是一个整数，本版本不认识的值是 `Unknown(原值)`。
///
/// 交给界面的 JSON 是每个变体给定的名字（camelCase 的字符串），不认识的值是原来的整数；
/// 从界面来的 JSON 两种都认。
///
/// ```ignore
/// open_enum! {
///     /// 被控端结束会话的原因
///     pub enum ByeReason {
///         /// 被控端的用户断开了所有控制端
///         Kicked = 0 => "kicked",
///         CodeChanged = 1 => "codeChanged",
///     }
/// }
/// ```
///
/// 编号一旦发布就不能改、不能复用（和消息里的编号一样）。
macro_rules! open_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$vmeta:meta])*
                $variant:ident = $code:literal => $json:literal,
            )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis enum $name {
            $(
                $(#[$vmeta])*
                $variant,
            )+
            /// 本版本不认识的值（对方版本新增的）：线上的原值。只由解码产生
            Unknown(u32),
        }

        impl $name {
            /// 本版本认识的所有值
            pub const KNOWN: &'static [$name] = &[$($name::$variant),+];

            /// 线上的编号
            pub const fn code(self) -> u32 {
                match self {
                    $($name::$variant => $code,)+
                    $name::Unknown(n) => n,
                }
            }

            pub const fn from_code(n: u32) -> Self {
                match n {
                    $($code => $name::$variant,)+
                    n => $name::Unknown(n),
                }
            }

            /// 本版本认识这个值
            pub const fn is_known(self) -> bool {
                !matches!(self, $name::Unknown(_))
            }

            /// JSON 里的名字；不认识的值没有名字
            pub const fn name(self) -> Option<&'static str> {
                match self {
                    $($name::$variant => Some($json),)+
                    $name::Unknown(_) => None,
                }
            }

            pub fn from_name(s: &str) -> Option<Self> {
                match s {
                    $($json => Some($name::$variant),)+
                    _ => None,
                }
            }
        }

        impl<C> ::minicbor::Encode<C> for $name {
            fn encode<W: ::minicbor::encode::Write>(
                &self,
                e: &mut ::minicbor::Encoder<W>,
                _: &mut C,
            ) -> Result<(), ::minicbor::encode::Error<W::Error>> {
                e.u32(self.code())?;
                Ok(())
            }
        }

        impl<'b, C> ::minicbor::Decode<'b, C> for $name {
            fn decode(
                d: &mut ::minicbor::Decoder<'b>,
                _: &mut C,
            ) -> Result<Self, ::minicbor::decode::Error> {
                Ok(Self::from_code(d.u32()?))
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                match self.name() {
                    Some(n) => s.serialize_str(n),
                    None => s.serialize_u32(self.code()),
                }
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl ::serde::de::Visitor<'_> for V {
                    type Value = $name;

                    fn expecting(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                        f.write_str(concat!("the name or number of a ", stringify!($name)))
                    }

                    fn visit_str<E: ::serde::de::Error>(self, v: &str) -> Result<$name, E> {
                        $name::from_name(v).ok_or_else(|| E::unknown_variant(v, &[$($json),+]))
                    }

                    fn visit_u64<E: ::serde::de::Error>(self, v: u64) -> Result<$name, E> {
                        u32::try_from(v).map($name::from_code).map_err(|_| {
                            E::invalid_value(::serde::de::Unexpected::Unsigned(v), &self)
                        })
                    }
                }
                d.deserialize_any(V)
            }
        }
    };
}
pub(crate) use open_enum;

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use minicbor::{Decode, Encode};

    open_enum! {
        enum Old {
            A = 0 => "a",
            B = 1 => "b",
        }
    }

    open_enum! {
        enum New {
            A = 0 => "a",
            B = 1 => "b",
            C = 2 => "c",
        }
    }

    /// 带字段的枚举（只能整个跳过）
    #[derive(Encode, Decode, Debug, PartialEq, Clone)]
    enum OldEvent {
        #[n(0)]
        Move {
            #[n(0)]
            x: u32,
        },
    }

    #[derive(Encode, Decode, Debug, PartialEq, Clone)]
    enum NewEvent {
        #[n(0)]
        Move {
            #[n(0)]
            x: u32,
        },
        #[n(1)]
        Pen {
            #[n(0)]
            pressure: u32,
        },
    }

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct OldMsg {
        #[n(0)]
        list: Vec<Old>,
        #[n(1)]
        one: Old,
        #[cbor(n(2), with = "crate::cbor::open_vec")]
        events: Vec<OldEvent>,
        #[cbor(n(3), with = "crate::cbor::opt", has_nil)]
        event: Option<OldEvent>,
        #[n(4)]
        after: u32,
    }

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct NewMsg {
        #[n(0)]
        list: Vec<New>,
        #[n(1)]
        one: New,
        #[cbor(n(2), with = "crate::cbor::open_vec")]
        events: Vec<NewEvent>,
        #[cbor(n(3), with = "crate::cbor::opt", has_nil)]
        event: Option<NewEvent>,
        #[n(4)]
        after: u32,
        #[n(5)]
        extra: Option<u32>,
    }

    /// 新版本加了枚举值、事件和字段：旧版本照样解得出来。开放枚举留着原值，事件只跳过不认识的那一个
    #[test]
    fn old_reads_new() {
        let wire = minicbor::to_vec(NewMsg {
            list: vec![New::B, New::C, New::A],
            one: New::C,
            events: vec![NewEvent::Pen { pressure: 1 }, NewEvent::Move { x: 2 }],
            event: Some(NewEvent::Pen { pressure: 3 }),
            after: 9,
            extra: Some(7),
        })
        .unwrap();
        let old: OldMsg = minicbor::decode(&wire).unwrap();
        assert_eq!(
            old,
            OldMsg {
                list: vec![Old::B, Old::Unknown(2), Old::A],
                one: Old::Unknown(2),
                events: vec![OldEvent::Move { x: 2 }],
                event: None,
                after: 9,
            }
        );
        // 新版本读旧版本：缺的 Option 字段是 None
        let wire = minicbor::to_vec(OldMsg {
            list: vec![Old::B],
            one: Old::B,
            events: vec![],
            event: Some(OldEvent::Move { x: 1 }),
            after: 2,
        })
        .unwrap();
        let new: NewMsg = minicbor::decode(&wire).unwrap();
        assert_eq!((new.list, new.one, new.extra), (vec![New::B], New::B, None));
        assert_eq!(new.event, Some(NewEvent::Move { x: 1 }));
    }

    /// 没有标明可以跳过的地方遇到不认识的变体：是错误，不是悄悄跳过、更不是错位
    #[test]
    fn unknown_variants_elsewhere_are_errors() {
        // 派生的 Option<结构体>，结构体里有一个带字段的枚举：派生代码会把它当成 None 并错位跳过。
        // 我们自己的跳过（open_vec）只认这一项本身的变体，更深处的变成普通错误，外层就吞不掉
        #[derive(Encode, Decode, Debug, PartialEq)]
        struct OldInner {
            #[n(0)]
            e: OldEvent,
        }
        #[derive(Encode, Decode, Debug, PartialEq)]
        struct NewInner {
            #[n(0)]
            e: NewEvent,
        }
        #[derive(Encode, Decode, Debug, PartialEq)]
        struct OldOuter {
            #[cbor(n(0), with = "crate::cbor::open_vec")]
            items: Vec<OldInner>,
            #[n(1)]
            a: u32,
            #[n(2)]
            b: u32,
        }
        #[derive(Encode, Decode, Debug, PartialEq)]
        struct NewOuter {
            #[cbor(n(0), with = "crate::cbor::open_vec")]
            items: Vec<NewInner>,
            #[n(1)]
            a: u32,
            #[n(2)]
            b: u32,
        }
        let wire = minicbor::to_vec(NewOuter {
            items: vec![NewInner {
                e: NewEvent::Pen { pressure: 1 },
            }],
            a: 2,
            b: 3,
        })
        .unwrap();
        let e = minicbor::decode::<OldOuter>(&wire).unwrap_err();
        assert!(!e.is_unknown_variant(), "{e}");

        #[derive(Encode, Decode, Debug, PartialEq)]
        struct OldOpt {
            #[n(0)]
            inner: Option<OldStrictInner>,
            #[n(1)]
            a: u32,
        }
        #[derive(Encode, Decode, Debug, PartialEq)]
        struct OldStrictInner {
            #[cbor(n(0), with = "crate::cbor::opt", has_nil)]
            e: Option<OldEvent>,
            #[n(1)]
            x: u32,
            #[n(2)]
            y: u32,
        }
        #[derive(Encode, Decode, Debug, PartialEq)]
        struct NewOpt {
            #[n(0)]
            inner: Option<NewStrictInner>,
            #[n(1)]
            a: u32,
        }
        #[derive(Encode, Decode, Debug, PartialEq)]
        struct NewStrictInner {
            #[cbor(n(0), with = "crate::cbor::opt", has_nil)]
            e: Option<NewEvent>,
            #[n(1)]
            x: u32,
            #[n(2)]
            y: u32,
        }
        // 标明了可以跳过（opt）：放在派生的 Option<结构体> 里也没问题，后面的字段照样对
        let wire = minicbor::to_vec(NewOpt {
            inner: Some(NewStrictInner {
                e: Some(NewEvent::Pen { pressure: 1 }),
                x: 2,
                y: 3,
            }),
            a: 4,
        })
        .unwrap();
        assert_eq!(
            minicbor::decode::<OldOpt>(&wire).unwrap(),
            OldOpt {
                inner: Some(OldStrictInner {
                    e: None,
                    x: 2,
                    y: 3
                }),
                a: 4
            }
        );
    }

    #[test]
    fn open_enums_on_the_wire_and_in_json() {
        assert_eq!(minicbor::to_vec(New::C).unwrap(), [2]);
        assert_eq!(minicbor::decode::<Old>(&[2]).unwrap(), Old::Unknown(2));
        assert_eq!(minicbor::to_vec(Old::Unknown(77)).unwrap(), [0x18, 77]);
        assert_eq!(Old::from_code(1), Old::B);
        assert_eq!((Old::B.code(), Old::Unknown(9).code()), (1, 9));
        assert!(Old::A.is_known() && !Old::Unknown(5).is_known());
        assert_eq!(Old::KNOWN, [Old::A, Old::B]);

        assert_eq!(
            serde_json::to_value(Old::B).unwrap(),
            serde_json::json!("b")
        );
        assert_eq!(
            serde_json::to_value(Old::Unknown(9)).unwrap(),
            serde_json::json!(9)
        );
        assert_eq!(serde_json::from_str::<Old>("\"a\"").unwrap(), Old::A);
        assert_eq!(serde_json::from_str::<Old>("1").unwrap(), Old::B);
        assert_eq!(serde_json::from_str::<Old>("7").unwrap(), Old::Unknown(7));
        assert!(serde_json::from_str::<Old>("\"z\"").is_err());
        assert!(serde_json::from_str::<Old>("-1").is_err());
    }

    #[test]
    fn variant_positions() {
        let wire = minicbor::to_vec(NewEvent::Pen { pressure: 5 }).unwrap();
        assert_eq!(super::variant_of(&wire), Some(1));
        let e = minicbor::decode::<OldEvent>(&wire).unwrap_err();
        assert!(super::unknown_here(&e, &wire));
        assert!(!super::strict(e).is_unknown_variant());
    }
}
