//! CBOR 编码的辅助：哪些地方怎样容忍对方版本里新增的东西（见 [`crate::framing`] 的"兼容规则"）。
//!
//! minicbor 的派生已经做到：不认识的字段跳过，缺了的 `Option` 字段是 `None`，`Option<枚举>` 遇到不认识的变体是 `None`。
//! 这里补上另外两种：
//!
//! * [`open_vec`]：列表里不认识的元素跳过（例如对方新增了一种编码格式，其余的照样认）；
//! * [`or_default`]：单个枚举值不认识时用默认值（例如对方新增了一种平台）；
//! * [`opt`]：`Option<枚举>` 遇到不认识的变体是 `None`。派生自带的处理对只有编号的枚举（`index_only`）
//!   是错的：它在解码失败之后再跳过一项，而这种枚举的编号就是整项，结果把后面一个字段也跳过了。
//!   所以 `Option<枚举>` 字段都用它：`#[cbor(n(i), with = "crate::cbor::opt", has_nil)]`。

use minicbor::data::Type;
use minicbor::decode::{Decoder, Error};
use minicbor::encode::{Encoder, Write};
use minicbor::{Decode, Encode};

/// 解码 `d` 接下来的一个值；不认识的枚举变体返回 `Ok(None)`（这个值整个跳过），其它错误照常返回
fn tolerant<'b, Ctx, T: Decode<'b, Ctx>>(
    d: &mut Decoder<'b>,
    ctx: &mut Ctx,
) -> Result<Option<T>, Error> {
    let input = d.input();
    let start = d.position();
    d.skip()?;
    let end = d.position();
    match T::decode(&mut Decoder::new(&input[start..end]), ctx) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.is_unknown_variant() => Ok(None),
        Err(e) => Err(e),
    }
}

/// 列表里不认识的元素（对方版本新增的枚举变体）跳过，其余照收。用法：`#[cbor(n(i), with = "crate::cbor::open_vec")]`
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

/// `Option<枚举>`：没有值（null）、或者不认识的变体都是 `None`（见模块说明）
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

/// 不认识的枚举值（对方版本新增的变体）换成默认值。用法：`#[cbor(n(i), with = "crate::cbor::or_default")]`
pub mod or_default {
    use super::*;

    pub fn encode<Ctx, T: Encode<Ctx>, W: Write>(
        v: &T,
        e: &mut Encoder<W>,
        ctx: &mut Ctx,
    ) -> Result<(), minicbor::encode::Error<W::Error>> {
        v.encode(e, ctx)
    }

    pub fn decode<'b, Ctx, T: Decode<'b, Ctx> + Default>(
        d: &mut Decoder<'b>,
        ctx: &mut Ctx,
    ) -> Result<T, Error> {
        Ok(tolerant(d, ctx)?.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use minicbor::{Decode, Encode};

    #[derive(Encode, Decode, Debug, PartialEq, Clone, Copy, Default)]
    #[cbor(index_only)]
    enum Old {
        #[default]
        #[n(0)]
        A,
        #[n(1)]
        B,
    }

    #[derive(Encode, Decode, Debug, PartialEq, Clone, Copy, Default)]
    #[cbor(index_only)]
    enum New {
        #[default]
        #[n(0)]
        A,
        #[n(1)]
        B,
        #[n(2)]
        C,
    }

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct OldMsg {
        #[cbor(n(0), with = "crate::cbor::open_vec")]
        list: Vec<Old>,
        #[cbor(n(1), with = "crate::cbor::or_default")]
        one: Old,
    }

    #[derive(Encode, Decode, Debug, PartialEq)]
    #[cbor(map)]
    struct OldMap {
        #[cbor(n(0), with = "crate::cbor::opt", has_nil)]
        a: Option<Old>,
        #[n(1)]
        b: u32,
    }

    #[derive(Encode, Decode, Debug, PartialEq)]
    #[cbor(map)]
    struct NewMap {
        #[n(0)]
        a: Option<New>,
        #[n(1)]
        b: u32,
    }

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct OldArray {
        #[cbor(n(0), with = "crate::cbor::opt", has_nil)]
        a: Option<Old>,
        #[n(1)]
        b: u32,
    }

    /// `Option<只有编号的枚举>` 遇到不认识的变体：是 None，后面的字段不受影响（见模块说明）
    #[test]
    fn unknown_index_only_variants_in_options() {
        let wire = minicbor::to_vec(NewMap {
            a: Some(New::C),
            b: 7,
        })
        .unwrap();
        assert_eq!(
            minicbor::decode::<OldMap>(&wire).unwrap(),
            OldMap { a: None, b: 7 }
        );
        let wire = minicbor::to_vec(NewMap {
            a: Some(New::B),
            b: 7,
        })
        .unwrap();
        assert_eq!(
            minicbor::decode::<OldMap>(&wire).unwrap(),
            OldMap {
                a: Some(Old::B),
                b: 7
            }
        );
        // None 不上线
        let wire = minicbor::to_vec(OldMap { a: None, b: 1 }).unwrap();
        assert_eq!(
            minicbor::decode::<NewMap>(&wire).unwrap(),
            NewMap { a: None, b: 1 }
        );
        let mut e = minicbor::Encoder::new(Vec::new());
        e.array(2).unwrap().u32(2).unwrap().u32(5).unwrap();
        assert_eq!(
            minicbor::decode::<OldArray>(&e.into_writer()).unwrap(),
            OldArray { a: None, b: 5 }
        );
    }

    #[derive(Encode, Decode, Debug, PartialEq)]
    struct NewMsg {
        #[cbor(n(0), with = "crate::cbor::open_vec")]
        list: Vec<New>,
        #[cbor(n(1), with = "crate::cbor::or_default")]
        one: New,
        #[n(2)]
        extra: Option<u32>,
    }

    /// 新版本加了枚举变体和字段：旧版本照样解得出来，不认识的跳过 / 用默认值
    #[test]
    fn old_reads_new() {
        let wire = minicbor::to_vec(NewMsg {
            list: vec![New::B, New::C, New::A],
            one: New::C,
            extra: Some(7),
        })
        .unwrap();
        let old: OldMsg = minicbor::decode(&wire).unwrap();
        assert_eq!(
            old,
            OldMsg {
                list: vec![Old::B, Old::A],
                one: Old::A
            }
        );
        // 新版本读旧版本：缺的 Option 字段是 None
        let wire = minicbor::to_vec(OldMsg {
            list: vec![Old::B],
            one: Old::B,
        })
        .unwrap();
        let new: NewMsg = minicbor::decode(&wire).unwrap();
        assert_eq!(new.list, [New::B]);
        assert_eq!(new.one, New::B);
        assert_eq!(new.extra, None);
    }
}
