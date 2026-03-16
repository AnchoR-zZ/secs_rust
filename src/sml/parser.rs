use nom::{
    branch::alt,
    bytes::complete::{tag, take_while},
    character::complete::{char, digit1, hex_digit1, multispace0, multispace1},
    combinator::{map, map_res, opt, recognize, value},
    multi::many0,
    sequence::{delimited, preceded},
    IResult,
    Parser,
};
use std::str::FromStr;

use crate::secs2::Secs2;
use super::error::SmlError;

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

// Parse String: "Hello"
fn parse_string_literal(input: &str) -> IResult<&str, String, SmlError<&str>> {
    let parser = delimited(
        char('"'),
        // Simple string parser, does not handle escaped quotes for now for simplicity
        take_while(|c| c != '"'), 
        char('"'),
    );
    map(parser, |s: &str| s.to_string()).parse(input)
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

fn parse_list(input: &str) -> IResult<&str, Secs2, SmlError<&str>> {
    let (input, _) = tag("<L").parse(input)?;
    // Optional length [n] - ignored for now as we just parse all children
    let (input, _) = opt(preceded(multispace1, digit1)).parse(input)?; 
    let (input, items) = many0(ws(parse_item)).parse(input)?;
    let (input, _) = char('>').parse(input)?;
    Ok((input, Secs2::LIST(items)))
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

fn parse_item(input: &str) -> IResult<&str, Secs2, SmlError<&str>> {
    alt((
        parse_list,
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
    let (input, body) = opt(ws(parse_item)).parse(input)?;
    
    // Optional trailing dot
    let (input, _) = opt(ws(char('.'))).parse(input)?;
    
    Ok((input, SmlMessage {
        stream,
        function,
        wait_bit,
        body,
    }))
}
