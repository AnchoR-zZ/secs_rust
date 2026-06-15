use nom::{
    branch::alt,
    bytes::complete::{escaped_transform, tag, take_while1},
    character::complete::{char, digit1, hex_digit1, multispace0},
    combinator::{map, map_res, opt, recognize, value},
    multi::many0,
    sequence::{delimited, preceded},
    IResult,
    Parser,
};
use std::str::FromStr;

use crate::secs2::Secs2;
use super::error::SmlError;

/// LIST 嵌套深度上限。用于防御递归下降解析器的栈溢出。
/// GEM 标准中最复杂的消息嵌套深度极少超过 5–6 层，64 层对所有合法用途宽松 10 倍以上，
/// 又能挡住任何恶意/损坏的深度嵌套导致的栈溢出。与 secs2 二进制解析器保持一致。
const MAX_SML_DEPTH: u32 = 64;

#[derive(Debug, PartialEq, Clone)]
pub struct SmlMessage {
    pub stream: u8,
    pub function: u8,
    pub wait_bit: bool,
    pub body: Option<Secs2>,
}

// Helper for whitespace
fn ws<'a, F, O, E>(inner: F) -> impl Parser<&'a str, Output = O, Error = E>
where
    F: Parser<&'a str, Output = O, Error = E>,
    E: nom::error::ParseError<&'a str>,
{
    delimited(multispace0, inner, multispace0)
}

// Parse Stream: S1, S2, etc.
fn parse_stream(input: &str) -> IResult<&str, u8, SmlError<&str>> {
    map_res(preceded(char('S'), digit1), |s: &str| s.parse::<u8>()).parse(input)
}

// Parse Function: F1, F2, etc.
fn parse_function(input: &str) -> IResult<&str, u8, SmlError<&str>> {
    map_res(preceded(char('F'), digit1), |s: &str| s.parse::<u8>()).parse(input)
}

// Parse Wait Bit: W
fn parse_wait_bit(input: &str) -> IResult<&str, bool, SmlError<&str>> {
    map(opt(ws(tag("W"))), |o| o.is_some()).parse(input)
}

fn parse_string_literal(input: &str) -> IResult<&str, String, SmlError<&str>> {
    let parser = escaped_transform(
        take_while1(|c: char| c != '"' && c != '\\'),
        '\\',
        alt((
            value("\\", tag("\\")),
            value("\"", tag("\"")),
            value("\n", tag("n")),
            value("\r", tag("r")),
            value("\t", tag("t")),
        )),
    );
    delimited(char('"'), parser, char('"')).parse(input)
}

// Parse Hex: 0x00
fn parse_hex_byte(input: &str) -> IResult<&str, u8, SmlError<&str>> {
    preceded(
        tag("0x"),
        map_res(hex_digit1, |s: &str| u8::from_str_radix(s, 16)),
    ).parse(input)
}

// Parse Boolean: T or F
fn parse_bool_val(input: &str) -> IResult<&str, bool, SmlError<&str>> {
    alt((
        value(true, tag("T")),
        value(false, tag("F")),
    )).parse(input)
}

// Generic number parser
fn parse_number<T: FromStr>(input: &str) -> IResult<&str, T, SmlError<&str>> {
    map_res(
        recognize((
            opt(char('-')),
            digit1,
            opt((char('.'), digit1)),
        )),
        |s: &str| s.parse::<T>(),
    ).parse(input)
}

// --- Item Parsers ---

fn parse_list(input: &str, depth: u32) -> IResult<&str, Secs2, SmlError<&str>> {
    // 深度守卫：递归进入 list 时检查，防止过深嵌套溢出栈。
    if depth > MAX_SML_DEPTH {
        return Err(nom::Err::Failure(SmlError::InvalidFormat(format!(
            "LIST nesting too deep: {} (max {})",
            depth, MAX_SML_DEPTH
        ))));
    }

    let (input, _) = tag("<L").parse(input)?;
    let (input, declared_len) = opt(preceded(
        multispace0,
        alt((
            delimited(char('['), map_res(digit1, |s: &str| s.parse::<usize>()), char(']')),
            map_res(digit1, |s: &str| s.parse::<usize>()),
        )),
    )).parse(input)?;

    // 手动循环解析子元素（原 many0(ws(parse_item))），并在每次递归时 depth + 1。
    // 遇到 '>' 表示 list 结束；其他情况继续解析子 item。
    let mut input = input;
    let mut items = Vec::new();
    loop {
        let (rest, _) = multispace0.parse(input)?;
        // 检查是否到达 list 结束符 '>'
        match char::<_, SmlError<&str>>('>').parse(rest) {
            Ok((after_close, _)) => {
                if let Some(expected) = declared_len {
                    if items.len() != expected {
                        return Err(nom::Err::Failure(SmlError::InvalidFormat(format!(
                            "List length marker says {} but found {} items",
                            expected,
                            items.len()
                        ))));
                    }
                }
                return Ok((after_close, Secs2::LIST(items)));
            }
            Err(nom::Err::Error(_)) => {
                // 不是 '>', 继续当作子 item 解析
            }
            Err(e) => return Err(e), // Failure / Incomplete 直接上抛
        }
        let (rest, item) = parse_item(rest, depth + 1)?;
        items.push(item);
        input = rest;
    }
}

fn parse_ascii(input: &str) -> IResult<&str, Secs2, SmlError<&str>> {
    let (input, _) = tag("<A").parse(input)?;
    let (input, s) = ws(parse_string_literal).parse(input)?;
    let (input, _) = char('>').parse(input)?;
    Ok((input, Secs2::ASCII(s)))
}

fn parse_binary(input: &str) -> IResult<&str, Secs2, SmlError<&str>> {
    let (input, _) = tag("<B").parse(input)?;
    let (input, bytes) = many0(ws(parse_hex_byte)).parse(input)?;
    let (input, _) = char('>').parse(input)?;
    Ok((input, Secs2::BINARY(bytes)))
}

fn parse_boolean(input: &str) -> IResult<&str, Secs2, SmlError<&str>> {
    let (input, _) = alt((tag("<Boolean"), tag("<BOOL"))).parse(input)?;
    let (input, bools) = many0(ws(parse_bool_val)).parse(input)?;
    let (input, _) = char('>').parse(input)?;
    Ok((input, Secs2::BOOLEAN(bools)))
}

// Numeric Macros
macro_rules! impl_numeric_parser {
    ($name:ident, $tag:expr, $type:ty, $variant:path) => {
        fn $name(input: &str) -> IResult<&str, Secs2, SmlError<&str>> {
            let (input, _) = tag($tag).parse(input)?;
            let (input, nums) = many0(ws(parse_number::<$type>)).parse(input)?;
            let (input, _) = char('>').parse(input)?;
            Ok((input, $variant(nums)))
        }
    };
}

impl_numeric_parser!(parse_u1, "<U1", u8, Secs2::U1);
impl_numeric_parser!(parse_u2, "<U2", u16, Secs2::U2);
impl_numeric_parser!(parse_u4, "<U4", u32, Secs2::U4);
impl_numeric_parser!(parse_u8, "<U8", u64, Secs2::U8);
impl_numeric_parser!(parse_i1, "<I1", i8, Secs2::I1);
impl_numeric_parser!(parse_i2, "<I2", i16, Secs2::I2);
impl_numeric_parser!(parse_i4, "<I4", i32, Secs2::I4);
impl_numeric_parser!(parse_i8, "<I8", i64, Secs2::I8);
impl_numeric_parser!(parse_f4, "<F4", f32, Secs2::D4);
impl_numeric_parser!(parse_f8, "<F8", f64, Secs2::D8);

// 构造一个把 depth 绑定到 parse_list 的解析器闭包。
// 用独立函数返回闭包是为了强制 HRTB（对任意输入生命周期成立），否则 alt 内
// 闭包的生命周期无法统一。nom 的 alt 需要每个分支都是 FnMut(&'a str) -> IResult<&'a str, _>。
fn list_parser_with_depth(depth: u32) -> impl FnMut(&str) -> IResult<&str, Secs2, SmlError<&str>> {
    move |i: &str| parse_list(i, depth)
}

// 顶层 item 解析器（depth = 0）。独立函数强制 HRTB，使闭包可用于 nom 组合子。
fn item_parser_root() -> impl FnMut(&str) -> IResult<&str, Secs2, SmlError<&str>> {
    move |i: &str| parse_item(i, 0)
}

fn parse_item(input: &str, depth: u32) -> IResult<&str, Secs2, SmlError<&str>> {
    alt((
        list_parser_with_depth(depth),
        parse_ascii,
        parse_binary,
        parse_boolean,
        parse_u1, parse_u2, parse_u4, parse_u8,
        parse_i1, parse_i2, parse_i4, parse_i8,
        parse_f4, parse_f8,
    )).parse(input)
}

// Top Level Parser
pub fn parse_sml(input: &str) -> IResult<&str, SmlMessage, SmlError<&str>> {
    let (input, _) = multispace0.parse(input)?;
    let (input, stream) = parse_stream(input)?;
    let (input, function) = parse_function(input)?;
    let (input, wait_bit) = parse_wait_bit(input)?;
    
    // Body is optional (e.g. S1F1 W .)
    let (input, body) = opt(ws(item_parser_root())).parse(input)?;
    
    // Optional trailing dot
    let (input, _) = opt(ws(char('.'))).parse(input)?;
    
    Ok((input, SmlMessage {
        stream,
        function,
        wait_bit,
        body,
    }))
}
