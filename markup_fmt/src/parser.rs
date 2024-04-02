//! This parser is designed for internal use,
//! not generating general-purpose AST.
//!
//! Also, the parser consumes string then produces AST directly without tokenizing.
//! For a formal parser, it should be:
//! `source -> tokens (produced by lexer/tokenizer) -> AST (produced by parser)`.
//! So, if you're learning or looking for a parser,
//! this is not a good example and you should look for other projects.

use crate::{
    ast::*,
    error::{SyntaxError, SyntaxErrorKind},
    helpers,
};
use std::{iter::Peekable, str::CharIndices};

#[derive(Clone, Debug)]
/// Supported languages.
pub enum Language {
    Html,
    Vue,
    Svelte,
    Astro,
    Jinja,
    Vento,
}

pub struct Parser<'s> {
    source: &'s str,
    language: Language,
    chars: Peekable<CharIndices<'s>>,
    state: ParserState,
}

#[derive(Default)]
struct ParserState {
    has_astro_front_matter: bool,
}

impl<'s> Parser<'s> {
    pub fn new(source: &'s str, language: Language) -> Self {
        Self {
            source,
            language,
            chars: source.char_indices().peekable(),
            state: Default::default(),
        }
    }

    fn try_parse<F, R>(&mut self, f: F) -> PResult<R>
    where
        F: FnOnce(&mut Self) -> PResult<R>,
    {
        let chars = self.chars.clone();
        let result = f(self);
        if result.is_err() {
            self.chars = chars;
        }
        result
    }

    fn emit_error(&mut self, kind: SyntaxErrorKind) -> SyntaxError {
        SyntaxError {
            kind,
            pos: self
                .chars
                .peek()
                .map(|(pos, _)| *pos)
                .unwrap_or(self.source.len()),
        }
    }

    fn skip_ws(&mut self) {
        while self
            .chars
            .next_if(|(_, c)| c.is_ascii_whitespace())
            .is_some()
        {}
    }

    fn parse_astro_attr(&mut self) -> PResult<AstroAttribute<'s>> {
        let name = if self.chars.next_if(|(_, c)| *c == '{').is_some() {
            None
        } else {
            let name = self.parse_attr_name()?;
            self.skip_ws();
            if self
                .chars
                .next_if(|(_, c)| *c == '=')
                .map(|_| self.skip_ws())
                .and_then(|_| self.chars.next_if(|(_, c)| *c == '{'))
                .is_some()
            {
                Some(name)
            } else {
                return Err(self.emit_error(SyntaxErrorKind::ExpectAstroAttr));
            }
        };

        self.parse_svelte_or_astro_expr()
            .map(|expr| AstroAttribute { name, expr })
    }

    fn parse_astro_expr(&mut self) -> PResult<AstroExpr<'s>> {
        if self.chars.next_if(|(_, c)| *c == '{').is_none() {
            return Err(self.emit_error(SyntaxErrorKind::ExpectAstroExpr));
        }

        let mut children = Vec::with_capacity(1);
        let mut braces_stack = 0u8;
        let mut pos = self
            .chars
            .peek()
            .map(|(i, _)| *i)
            .unwrap_or(self.source.len());
        while let Some((i, c)) = self.chars.peek() {
            match c {
                '{' => {
                    braces_stack += 1;
                    self.chars.next();
                }
                '}' => {
                    let i = *i;
                    self.chars.next();
                    if braces_stack == 0 {
                        debug_assert!(matches!(
                            children.last(),
                            Some(AstroExprChild::Template(..)) | None
                        ));
                        children.push(AstroExprChild::Script(unsafe {
                            self.source.get_unchecked(pos..i)
                        }));
                        break;
                    }
                    braces_stack -= 1;
                }
                '<' => {
                    let i = *i;
                    let mut chars = self.chars.clone();
                    chars.next();
                    if chars
                        .next_if(|(_, c)| is_tag_name_char(*c) || *c == '!' || *c == '>')
                        .is_some()
                    {
                        let prev = unsafe { self.source.get_unchecked(pos..i) };
                        if prev.is_empty() {
                            // do nothing
                        } else if prev.chars().all(|c| c.is_ascii_whitespace()) {
                            if let Some(AstroExprChild::Template(nodes)) = children.last_mut() {
                                nodes.push(Node::TextNode(TextNode {
                                    raw: prev,
                                    line_breaks: prev.chars().filter(|c| *c == '\n').count(),
                                }));
                            }
                        } else {
                            children.push(AstroExprChild::Script(prev));
                        }

                        let node = self.parse_node()?;
                        if let Some(AstroExprChild::Template(nodes)) = children.last_mut() {
                            nodes.push(node);
                        } else {
                            debug_assert!(matches!(
                                children.last(),
                                Some(AstroExprChild::Script(..)) | None
                            ));
                            children.push(AstroExprChild::Template(vec![node]));
                        }
                        pos = self
                            .chars
                            .peek()
                            .map(|(i, _)| *i)
                            .unwrap_or(self.source.len());
                    } else {
                        self.chars.next();
                    }
                }
                _ => {
                    self.chars.next();
                }
            }
        }

        Ok(AstroExpr { children })
    }

    fn parse_astro_front_matter(&mut self) -> PResult<AstroFrontMatter<'s>> {
        let Some((start, _)) = self
            .chars
            .next_if(|(_, c)| *c == '-')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '-'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '-'))
        else {
            return Err(self.emit_error(SyntaxErrorKind::ExpectAstroFrontMatter));
        };
        let start = start + 1;

        let mut pair_stack = vec![];
        let mut end = start;
        loop {
            match self.chars.next() {
                Some((i, '-')) if pair_stack.is_empty() => {
                    let mut chars = self.chars.clone();
                    if chars
                        .next_if(|(_, c)| *c == '-')
                        .and_then(|_| chars.next_if(|(_, c)| *c == '-'))
                        .is_some()
                    {
                        end = i;
                        self.chars = chars;
                        break;
                    }
                }
                Some((_, c @ '\'' | c @ '"' | c @ '`')) => {
                    if pair_stack.last().map(|last| *last == c).unwrap_or_default() {
                        pair_stack.pop();
                    } else if matches!(pair_stack.last(), Some('$' | '{') | None) {
                        pair_stack.push(c);
                    }
                }
                Some((_, '$')) if matches!(pair_stack.last(), Some('`')) => {
                    if self.chars.next_if(|(_, c)| *c == '{').is_some() {
                        pair_stack.push('$');
                    }
                }
                Some((_, '{')) if matches!(pair_stack.last(), Some('$' | '{') | None) => {
                    pair_stack.push('{');
                }
                Some((_, '}')) if matches!(pair_stack.last(), Some('$' | '{')) => {
                    pair_stack.pop();
                }
                Some((_, '/')) if matches!(pair_stack.last(), Some('$' | '}') | None) => {
                    if let Some((_, c)) = self.chars.next_if(|(_, c)| *c == '/' || *c == '*') {
                        pair_stack.push(c);
                    }
                }
                Some((_, '\n')) => {
                    if let Some('/') = pair_stack.last() {
                        pair_stack.pop();
                    }
                }
                Some((_, '*')) => {
                    if self
                        .chars
                        .next_if(|(_, c)| *c == '/' && matches!(pair_stack.last(), Some('*')))
                        .is_some()
                    {
                        pair_stack.pop();
                    }
                }
                Some((_, '\\')) if matches!(pair_stack.last(), Some('\'' | '"' | '`')) => {
                    self.chars.next();
                }
                Some(..) => continue,
                None => break,
            }
        }

        self.state.has_astro_front_matter = true;
        Ok(AstroFrontMatter {
            raw: unsafe { self.source.get_unchecked(start..end) },
        })
    }

    fn parse_attr(&mut self) -> PResult<Attribute<'s>> {
        match self.language {
            Language::Html | Language::Jinja | Language::Vento => {
                self.parse_native_attr().map(Attribute::NativeAttribute)
            }
            Language::Vue => self
                .try_parse(Parser::parse_vue_directive)
                .map(Attribute::VueDirective)
                .or_else(|_| self.parse_native_attr().map(Attribute::NativeAttribute)),
            Language::Svelte => self
                .try_parse(Parser::parse_svelte_attr)
                .map(Attribute::SvelteAttribute)
                .or_else(|_| self.parse_native_attr().map(Attribute::NativeAttribute)),
            Language::Astro => self
                .try_parse(Parser::parse_astro_attr)
                .map(Attribute::AstroAttribute)
                .or_else(|_| self.parse_native_attr().map(Attribute::NativeAttribute)),
        }
    }

    fn parse_attr_name(&mut self) -> PResult<&'s str> {
        let Some((start, _)) = self.chars.next_if(|(_, c)| is_attr_name_char(*c)) else {
            return Err(self.emit_error(SyntaxErrorKind::ExpectAttrName));
        };
        let mut end = start;

        while let Some((i, _)) = self.chars.next_if(|(_, c)| is_attr_name_char(*c)) {
            end = i;
        }

        unsafe { Ok(self.source.get_unchecked(start..=end)) }
    }

    fn parse_attr_value(&mut self) -> PResult<&'s str> {
        let quote = self.chars.next_if(|(_, c)| *c == '"' || *c == '\'');

        if let Some((start, quote)) = quote {
            let start = start + 1;
            let mut end = start;
            loop {
                match self.chars.next() {
                    Some((i, c)) if c == quote => {
                        end = i;
                        break;
                    }
                    Some(..) => continue,
                    None => break,
                }
            }
            Ok(unsafe { self.source.get_unchecked(start..end) })
        } else {
            fn is_unquoted_attr_value_char(c: char) -> bool {
                !c.is_ascii_whitespace() && !matches!(c, '"' | '\'' | '=' | '<' | '>' | '`')
            }

            let Some((start, _)) = self.chars.next_if(|(_, c)| is_unquoted_attr_value_char(*c))
            else {
                return Err(self.emit_error(SyntaxErrorKind::ExpectAttrValue));
            };
            let mut end = start;

            while let Some((i, _)) = self.chars.next_if(|(_, c)| is_unquoted_attr_value_char(*c)) {
                end = i;
            }

            unsafe { Ok(self.source.get_unchecked(start..=end)) }
        }
    }

    fn parse_comment(&mut self) -> PResult<Comment<'s>> {
        let Some((start, _)) = self
            .chars
            .next_if(|(_, c)| *c == '<')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '!'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '-'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '-'))
        else {
            return Err(self.emit_error(SyntaxErrorKind::ExpectComment));
        };
        let start = start + 1;

        let mut end = start;
        loop {
            match self.chars.next() {
                Some((i, '-')) => {
                    let mut chars = self.chars.clone();
                    if chars
                        .next_if(|(_, c)| *c == '-')
                        .and_then(|_| chars.next_if(|(_, c)| *c == '>'))
                        .is_some()
                    {
                        end = i;
                        self.chars = chars;
                        break;
                    }
                }
                Some(..) => continue,
                None => break,
            }
        }

        Ok(Comment {
            raw: unsafe { self.source.get_unchecked(start..end) },
        })
    }

    fn parse_doctype(&mut self) -> PResult<()> {
        if self
            .chars
            .next_if(|(_, c)| *c == '<')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '!'))
            .and_then(|_| self.chars.next_if(|(_, c)| c.eq_ignore_ascii_case(&'d')))
            .and_then(|_| self.chars.next_if(|(_, c)| c.eq_ignore_ascii_case(&'o')))
            .and_then(|_| self.chars.next_if(|(_, c)| c.eq_ignore_ascii_case(&'c')))
            .and_then(|_| self.chars.next_if(|(_, c)| c.eq_ignore_ascii_case(&'t')))
            .and_then(|_| self.chars.next_if(|(_, c)| c.eq_ignore_ascii_case(&'y')))
            .and_then(|_| self.chars.next_if(|(_, c)| c.eq_ignore_ascii_case(&'p')))
            .and_then(|_| self.chars.next_if(|(_, c)| c.eq_ignore_ascii_case(&'e')))
            .is_none()
        {
            return Err(self.emit_error(SyntaxErrorKind::ExpectDoctype));
        };
        self.skip_ws();

        if self
            .chars
            .next_if(|(_, c)| c.eq_ignore_ascii_case(&'h'))
            .and_then(|_| self.chars.next_if(|(_, c)| c.eq_ignore_ascii_case(&'t')))
            .and_then(|_| self.chars.next_if(|(_, c)| c.eq_ignore_ascii_case(&'m')))
            .and_then(|_| self.chars.next_if(|(_, c)| c.eq_ignore_ascii_case(&'l')))
            .is_none()
        {
            return Err(self.emit_error(SyntaxErrorKind::ExpectDoctype));
        }
        self.skip_ws();

        if self.chars.next_if(|(_, c)| *c == '>').is_some() {
            Ok(())
        } else {
            Err(self.emit_error(SyntaxErrorKind::ExpectDoctype))
        }
    }

    fn parse_element(&mut self) -> PResult<Element<'s>> {
        let Some(..) = self.chars.next_if(|(_, c)| *c == '<') else {
            return Err(self.emit_error(SyntaxErrorKind::ExpectElement));
        };
        let tag_name = self.parse_tag_name()?;
        let void_element = helpers::is_void_element(tag_name, self.language.clone());

        let mut attrs = vec![];
        let mut first_attr_same_line = true;
        loop {
            match self.chars.peek() {
                Some((_, '/')) => {
                    self.chars.next();
                    if self.chars.next_if(|(_, c)| *c == '>').is_some() {
                        return Ok(Element {
                            tag_name,
                            attrs,
                            first_attr_same_line,
                            children: vec![],
                            self_closing: true,
                            void_element,
                        });
                    }
                    return Err(self.emit_error(SyntaxErrorKind::ExpectSelfCloseTag));
                }
                Some((_, '>')) => {
                    self.chars.next();
                    if void_element {
                        return Ok(Element {
                            tag_name,
                            attrs,
                            first_attr_same_line,
                            children: vec![],
                            self_closing: false,
                            void_element,
                        });
                    }
                    break;
                }
                Some((_, '\n')) => {
                    if attrs.is_empty() {
                        first_attr_same_line = false;
                    }
                    self.chars.next();
                }
                Some((_, c)) if c.is_ascii_whitespace() => {
                    self.chars.next();
                }
                _ => {
                    attrs.push(self.parse_attr()?);
                }
            }
        }

        let mut children = vec![];
        if tag_name.eq_ignore_ascii_case("script")
            || tag_name.eq_ignore_ascii_case("style")
            || tag_name.eq_ignore_ascii_case("pre")
            || tag_name.eq_ignore_ascii_case("textarea")
        {
            let text_node = self.parse_raw_text_node(tag_name)?;
            if !text_node.raw.is_empty() {
                children.push(Node::TextNode(text_node));
            }
        }

        loop {
            match self.chars.peek() {
                Some((_, '<')) => {
                    let mut chars = self.chars.clone();
                    chars.next();
                    if let Some((pos, _)) = chars.next_if(|(_, c)| *c == '/') {
                        self.chars = chars;
                        let close_tag_name = self.parse_tag_name()?;
                        if !close_tag_name.eq_ignore_ascii_case(tag_name) {
                            return Err(SyntaxError {
                                kind: SyntaxErrorKind::ExpectCloseTag,
                                pos,
                            });
                        }
                        self.skip_ws();
                        if self.chars.next_if(|(_, c)| *c == '>').is_some() {
                            break;
                        }
                        return Err(self.emit_error(SyntaxErrorKind::ExpectCloseTag));
                    }
                    children.push(self.parse_node()?);
                }
                Some(..) => {
                    children.push(
                        if tag_name.eq_ignore_ascii_case("script")
                            || tag_name.eq_ignore_ascii_case("style")
                            || tag_name.eq_ignore_ascii_case("pre")
                            || tag_name.eq_ignore_ascii_case("textarea")
                        {
                            self.parse_raw_text_node(tag_name).map(Node::TextNode)?
                        } else {
                            self.parse_node()?
                        },
                    );
                }
                None => return Err(self.emit_error(SyntaxErrorKind::ExpectCloseTag)),
            }
        }

        Ok(Element {
            tag_name,
            attrs,
            first_attr_same_line,
            children,
            self_closing: false,
            void_element,
        })
    }

    fn parse_identifier(&mut self) -> PResult<&'s str> {
        fn is_identifier_char(c: char) -> bool {
            c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii() || c == '\\'
        }

        let Some((start, _)) = self.chars.next_if(|(_, c)| is_identifier_char(*c)) else {
            return Err(self.emit_error(SyntaxErrorKind::ExpectIdentifier));
        };
        let mut end = start;

        while let Some((i, _)) = self.chars.next_if(|(_, c)| is_identifier_char(*c)) {
            end = i;
        }

        unsafe { Ok(self.source.get_unchecked(start..=end)) }
    }

    /// This will consume the open and close char.
    fn parse_inside(&mut self, open: char, close: char, inclusive: bool) -> PResult<&'s str> {
        let Some(start) = self.chars.next_if(|(_, c)| *c == open).map(|(i, c)| {
            if inclusive {
                i
            } else {
                i + c.len_utf8()
            }
        }) else {
            return Err(self.emit_error(SyntaxErrorKind::ExpectChar(open)));
        };
        let mut end = start;
        let mut stack = 0u8;
        for (i, c) in self.chars.by_ref() {
            if c == open {
                stack += 1;
            } else if c == close {
                if stack == 0 {
                    end = if inclusive { i + close.len_utf8() } else { i };
                    break;
                }
                stack -= 1;
            }
        }
        Ok(unsafe { self.source.get_unchecked(start..end) })
    }

    fn parse_jinja_block_children(&mut self) -> PResult<Vec<Node<'s>>> {
        let mut children = vec![];
        loop {
            match self.chars.peek() {
                Some((_, '{')) => {
                    let mut chars = self.chars.clone();
                    chars.next();
                    if chars.next_if(|(_, c)| *c == '%').is_some() {
                        break;
                    }
                    children.push(self.parse_node()?);
                }
                Some(..) => {
                    children.push(self.parse_node()?);
                }
                None => return Err(self.emit_error(SyntaxErrorKind::ExpectJinjaBlockEnd)),
            }
        }
        Ok(children)
    }

    fn parse_jinja_comment(&mut self) -> PResult<JinjaComment<'s>> {
        let Some((start, _)) = self
            .chars
            .next_if(|(_, c)| *c == '{')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '#'))
        else {
            return Err(self.emit_error(SyntaxErrorKind::ExpectComment));
        };
        let start = start + 1;

        let mut end = start;
        loop {
            match self.chars.next() {
                Some((i, '#')) => {
                    let mut chars = self.chars.clone();
                    if chars.next_if(|(_, c)| *c == '}').is_some() {
                        end = i;
                        self.chars = chars;
                        break;
                    }
                }
                Some(..) => continue,
                None => break,
            }
        }

        Ok(JinjaComment {
            raw: unsafe { self.source.get_unchecked(start..end) },
        })
    }

    fn parse_jinja_tag(&mut self) -> PResult<JinjaTag<'s>> {
        let Some((start, _)) = self
            .chars
            .next_if(|(_, c)| *c == '{')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '%'))
        else {
            return Err(self.emit_error(SyntaxErrorKind::ExpectJinjaTag));
        };
        let start = start + 1;

        let mut end = start;
        loop {
            match self.chars.next() {
                Some((i, '%')) => {
                    if self.chars.next_if(|(_, c)| *c == '}').is_some() {
                        end = i;
                        break;
                    }
                }
                Some(..) => continue,
                None => break,
            }
        }

        Ok(JinjaTag {
            content: unsafe { self.source.get_unchecked(start..end) },
        })
    }

    fn parse_jinja_tag_or_block(&mut self, first_tag: Option<JinjaTag<'s>>) -> PResult<Node<'s>> {
        let first_tag = if let Some(first_tag) = first_tag {
            first_tag
        } else {
            self.parse_jinja_tag()?
        };
        let tag_name = parse_jinja_tag_name(&first_tag);

        if matches!(
            tag_name,
            "for"
                | "if"
                | "macro"
                | "call"
                | "filter"
                | "block"
                | "apply"
                | "autoescape"
                | "embed"
                | "with"
        ) {
            let mut body = vec![JinjaTagOrChildren::Tag(first_tag)];

            loop {
                let children = self.parse_jinja_block_children()?;
                if !children.is_empty() {
                    body.push(JinjaTagOrChildren::Children(children));
                }
                if let Ok(next_tag) = self.parse_jinja_tag() {
                    let next_tag_name = parse_jinja_tag_name(&next_tag);
                    if next_tag_name
                        .strip_prefix("end")
                        .map(|name| name == tag_name)
                        .unwrap_or_default()
                    {
                        body.push(JinjaTagOrChildren::Tag(next_tag));
                        break;
                    }
                    if (tag_name == "if" || tag_name == "for")
                        && matches!(next_tag_name, "elif" | "elseif" | "else")
                    {
                        body.push(JinjaTagOrChildren::Tag(next_tag));
                    } else {
                        body.push(JinjaTagOrChildren::Children(vec![
                            self.parse_jinja_tag_or_block(Some(next_tag))?
                        ]));
                    }
                } else {
                    break;
                }
            }
            Ok(Node::JinjaBlock(JinjaBlock { body }))
        } else {
            Ok(Node::JinjaTag(first_tag))
        }
    }

    fn parse_mustache_interpolation(&mut self) -> PResult<&'s str> {
        let Some((start, _)) = self
            .chars
            .next_if(|(_, c)| *c == '{')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '{'))
        else {
            return Err(self.emit_error(SyntaxErrorKind::ExpectMustacheInterpolation));
        };
        let start = start + 1;

        let mut end = start;
        loop {
            match self.chars.next() {
                Some((i, '}')) => {
                    if self.chars.next_if(|(_, c)| *c == '}').is_some() {
                        end = i;
                        break;
                    }
                }
                Some(..) => continue,
                None => break,
            }
        }

        Ok(unsafe { self.source.get_unchecked(start..end) })
    }

    fn parse_native_attr(&mut self) -> PResult<NativeAttribute<'s>> {
        let name = self.parse_attr_name()?;
        self.skip_ws();
        let value = if self.chars.next_if(|(_, c)| *c == '=').is_some() {
            self.skip_ws();
            Some(self.parse_attr_value()?)
        } else {
            None
        };
        Ok(NativeAttribute { name, value })
    }

    fn parse_node(&mut self) -> PResult<Node<'s>> {
        match self.chars.peek() {
            Some((_, '<')) => {
                let mut chars = self.chars.clone();
                chars.next();
                match chars.next() {
                    Some((_, c)) if is_tag_name_char(c) => self.parse_element().map(Node::Element),
                    Some((_, '!')) => {
                        if matches!(
                            self.language,
                            Language::Html | Language::Astro | Language::Jinja | Language::Vento
                        ) {
                            self.try_parse(Parser::parse_comment)
                                .map(Node::Comment)
                                .or_else(|_| {
                                    self.try_parse(Parser::parse_doctype).map(|_| Node::Doctype)
                                })
                                .or_else(|_| self.parse_text_node().map(Node::TextNode))
                        } else {
                            self.parse_comment().map(Node::Comment)
                        }
                    }
                    Some((_, '>')) if matches!(self.language, Language::Astro) => {
                        self.parse_element().map(Node::Element)
                    }
                    _ => self.parse_text_node().map(Node::TextNode),
                }
            }
            Some((_, '{')) => {
                let mut chars = self.chars.clone();
                chars.next();
                match chars.next() {
                    Some((_, '{')) if matches!(self.language, Language::Vue | Language::Jinja) => {
                        self.parse_mustache_interpolation()
                            .map(|expr| match self.language {
                                Language::Vue => Node::VueInterpolation(VueInterpolation { expr }),
                                Language::Jinja => {
                                    Node::JinjaInterpolation(JinjaInterpolation { expr })
                                }
                                _ => unreachable!(),
                            })
                    }
                    Some((_, '{')) if matches!(self.language, Language::Vento) => {
                        self.parse_vento_tag_or_block(None)
                    }
                    Some((pos, '#')) if matches!(self.language, Language::Svelte) => self
                        .try_parse(Parser::parse_svelte_if_block)
                        .map(Node::SvelteIfBlock)
                        .or_else(|_| {
                            self.try_parse(Parser::parse_svelte_each_block)
                                .map(Node::SvelteEachBlock)
                        })
                        .or_else(|_| {
                            self.try_parse(Parser::parse_svelte_await_block)
                                .map(Node::SvelteAwaitBlock)
                        })
                        .or_else(|_| {
                            self.try_parse(Parser::parse_svelte_key_block)
                                .map(Node::SvelteKeyBlock)
                        })
                        .map_err(|_| SyntaxError {
                            kind: SyntaxErrorKind::UnknownSvelteBlock,
                            pos,
                        }),
                    Some((_, '#')) if matches!(self.language, Language::Jinja) => {
                        self.parse_jinja_comment().map(Node::JinjaComment)
                    }
                    Some((_, '@')) => self.parse_svelte_at_tag().map(Node::SvelteAtTag),
                    Some((_, '%')) if matches!(self.language, Language::Jinja) => {
                        self.parse_jinja_tag_or_block(None)
                    }
                    _ => match self.language {
                        Language::Svelte => self
                            .parse_svelte_interpolation()
                            .map(Node::SvelteInterpolation),
                        Language::Astro => self.parse_astro_expr().map(Node::AstroExpr),
                        _ => self.parse_text_node().map(Node::TextNode),
                    },
                }
            }
            Some((_, '-'))
                if matches!(self.language, Language::Astro)
                    && !self.state.has_astro_front_matter =>
            {
                let mut chars = self.chars.clone();
                chars.next();
                if let Some(((_, '-'), (_, '-'))) = chars.next().zip(chars.next()) {
                    self.parse_astro_front_matter().map(Node::AstroFrontMatter)
                } else {
                    self.parse_text_node().map(Node::TextNode)
                }
            }
            Some(..) => self.parse_text_node().map(Node::TextNode),
            None => Err(self.emit_error(SyntaxErrorKind::ExpectElement)),
        }
    }

    fn parse_raw_text_node(&mut self, tag_name: &str) -> PResult<TextNode<'s>> {
        let start = self
            .chars
            .peek()
            .map(|(i, _)| *i)
            .unwrap_or(self.source.len());

        let mut line_breaks = 0;
        let end;
        loop {
            match self.chars.peek() {
                Some((i, '<')) => {
                    let i = *i;
                    let mut chars = self.chars.clone();
                    chars.next();
                    if chars
                        .next_if(|(_, c)| *c == '/')
                        .map(|_| {
                            chars
                                .zip(tag_name.chars())
                                .all(|((_, a), b)| a.eq_ignore_ascii_case(&b))
                        })
                        .unwrap_or_default()
                    {
                        end = i;
                        break;
                    }
                    self.chars.next();
                }
                Some((_, c)) => {
                    if *c == '\n' {
                        line_breaks += 1;
                    }
                    self.chars.next();
                }
                None => {
                    end = self.source.len();
                    break;
                }
            }
        }

        Ok(TextNode {
            raw: unsafe { self.source.get_unchecked(start..end) },
            line_breaks,
        })
    }

    pub fn parse_root(&mut self) -> PResult<Root<'s>> {
        let mut children = vec![];
        while self.chars.peek().is_some() {
            children.push(self.parse_node()?);
        }

        Ok(Root { children })
    }

    fn parse_svelte_at_tag(&mut self) -> PResult<SvelteAtTag<'s>> {
        if self
            .chars
            .next_if(|(_, c)| *c == '{')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '@'))
            .is_none()
        {
            return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteAtTag));
        };
        let name = self.parse_identifier()?;
        self.skip_ws();
        let expr = self.parse_svelte_or_astro_expr()?;
        Ok(SvelteAtTag { name, expr })
    }

    fn parse_svelte_attr(&mut self) -> PResult<SvelteAttribute<'s>> {
        let name = if self.chars.next_if(|(_, c)| *c == '{').is_some() {
            None
        } else {
            let name = self.parse_attr_name()?;
            self.skip_ws();
            if self
                .chars
                .next_if(|(_, c)| *c == '=')
                .map(|_| self.skip_ws())
                .and_then(|_| self.chars.next_if(|(_, c)| *c == '{'))
                .is_some()
            {
                Some(name)
            } else {
                return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteAttr));
            }
        };

        self.parse_svelte_or_astro_expr()
            .map(|expr| SvelteAttribute { name, expr })
    }

    fn parse_svelte_await_block(&mut self) -> PResult<Box<SvelteAwaitBlock<'s>>> {
        if self
            .chars
            .next_if(|(_, c)| *c == '{')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '#'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'a'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'w'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'a'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'i'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 't'))
            .and_then(|_| self.chars.next_if(|(_, c)| c.is_ascii_whitespace()))
            .is_none()
        {
            return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteIfBlock));
        };
        self.skip_ws();

        let expr = {
            let start = self
                .chars
                .peek()
                .map(|(i, _)| *i)
                .unwrap_or(self.source.len());
            let mut end = start;
            let mut braces_stack = 0u8;
            loop {
                match self.chars.peek() {
                    Some((i, c)) if c.is_ascii_whitespace() => {
                        let i = *i;
                        self.skip_ws();
                        let mut chars = self.chars.clone();
                        match chars.next() {
                            Some((_, 't')) => {
                                if chars
                                    .next_if(|(_, c)| *c == 'h')
                                    .and_then(|_| chars.next_if(|(_, c)| *c == 'e'))
                                    .and_then(|_| chars.next_if(|(_, c)| *c == 'n'))
                                    .is_some()
                                {
                                    end = i;
                                    break;
                                }
                            }
                            Some((_, 'c')) => {
                                if chars
                                    .next_if(|(_, c)| *c == 'a')
                                    .and_then(|_| chars.next_if(|(_, c)| *c == 't'))
                                    .and_then(|_| chars.next_if(|(_, c)| *c == 'c'))
                                    .and_then(|_| chars.next_if(|(_, c)| *c == 'h'))
                                    .is_some()
                                {
                                    end = i;
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    Some((i, '{')) => {
                        braces_stack += 1;
                        end = *i;
                        self.chars.next();
                    }
                    Some((i, '}')) => {
                        end = *i;
                        if braces_stack == 0 {
                            break;
                        }
                        self.chars.next();
                        braces_stack -= 1;
                    }
                    Some((i, _)) => {
                        end = *i;
                        self.chars.next();
                    }
                    None => break,
                }
            }
            unsafe { self.source.get_unchecked(start..end) }
        };

        self.skip_ws();
        let then_binding = if self
            .chars
            .next_if(|(_, c)| *c == 't')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'h'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'e'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'n'))
            .is_some()
        {
            self.skip_ws();
            Some(self.parse_svelte_binding()?)
        } else {
            None
        };

        self.skip_ws();
        let catch_binding = if self
            .chars
            .next_if(|(_, c)| *c == 'c')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'a'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 't'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'c'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'h'))
            .is_some()
        {
            self.skip_ws();
            Some(self.parse_svelte_binding()?)
        } else {
            None
        };

        self.skip_ws();
        if self.chars.next_if(|(_, c)| *c == '}').is_none() {
            return Err(self.emit_error(SyntaxErrorKind::ExpectChar('}')));
        }

        let children = self.parse_svelte_block_children()?;

        let then_block = if self
            .try_parse(|parser| {
                parser
                    .chars
                    .next_if(|(_, c)| *c == '{')
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == ':'))
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == 't'))
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == 'h'))
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == 'e'))
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == 'n'))
                    .ok_or_else(|| parser.emit_error(SyntaxErrorKind::ExpectSvelteThenBlock))
            })
            .is_ok()
        {
            self.skip_ws();
            let binding = self.parse_svelte_binding()?;
            self.skip_ws();
            if self.chars.next_if(|(_, c)| *c == '}').is_none() {
                return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteThenBlock));
            }
            let children = self.parse_svelte_block_children()?;
            Some(SvelteThenBlock { binding, children })
        } else {
            None
        };

        let catch_block = if self
            .try_parse(|parser| {
                parser
                    .chars
                    .next_if(|(_, c)| *c == '{')
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == ':'))
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == 'c'))
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == 'a'))
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == 't'))
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == 'c'))
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == 'h'))
                    .ok_or_else(|| parser.emit_error(SyntaxErrorKind::ExpectSvelteCatchBlock))
            })
            .is_ok()
        {
            self.skip_ws();
            let binding = match self.chars.peek() {
                Some((_, '}')) => None,
                _ => Some(self.parse_svelte_binding()?),
            };
            self.skip_ws();
            if self.chars.next_if(|(_, c)| *c == '}').is_none() {
                return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteCatchBlock));
            }
            let children = self.parse_svelte_block_children()?;
            Some(SvelteCatchBlock { binding, children })
        } else {
            None
        };

        if self
            .chars
            .next_if(|(_, c)| *c == '{')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '/'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'a'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'w'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'a'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'i'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 't'))
            .map(|_| self.skip_ws())
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '}'))
            .is_some()
        {
            Ok(Box::new(SvelteAwaitBlock {
                expr,
                then_binding,
                catch_binding,
                children,
                then_block,
                catch_block,
            }))
        } else {
            Err(self.emit_error(SyntaxErrorKind::ExpectSvelteBlockEnd))
        }
    }

    fn parse_svelte_binding(&mut self) -> PResult<&'s str> {
        match self.chars.peek() {
            Some((_, '{')) => self.parse_inside('{', '}', true),
            Some((_, '[')) => self.parse_inside('[', ']', true),
            _ => self.parse_identifier(),
        }
    }

    fn parse_svelte_block_children(&mut self) -> PResult<Vec<Node<'s>>> {
        let mut children = vec![];
        loop {
            match self.chars.peek() {
                Some((_, '{')) => {
                    let mut chars = self.chars.clone();
                    chars.next();
                    if chars.next_if(|(_, c)| *c == '/' || *c == ':').is_some() {
                        break;
                    }
                    children.push(self.parse_node()?);
                }
                Some(..) => {
                    children.push(self.parse_node()?);
                }
                None => return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteBlockEnd)),
            }
        }
        Ok(children)
    }

    fn parse_svelte_each_block(&mut self) -> PResult<SvelteEachBlock<'s>> {
        if self
            .chars
            .next_if(|(_, c)| *c == '{')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '#'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'e'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'a'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'c'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'h'))
            .and_then(|_| self.chars.next_if(|(_, c)| c.is_ascii_whitespace()))
            .is_none()
        {
            return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteIfBlock));
        };
        self.skip_ws();

        let expr = {
            let start = self
                .chars
                .peek()
                .map(|(i, _)| *i)
                .unwrap_or(self.source.len());
            let mut end = start;
            loop {
                match self.chars.peek() {
                    Some((i, c)) if c.is_ascii_whitespace() => {
                        end = *i;
                        self.skip_ws();
                        let mut chars = self.chars.clone();
                        if chars
                            .next_if(|(_, c)| *c == 'a')
                            .and_then(|_| chars.next_if(|(_, c)| *c == 's'))
                            .is_some()
                        {
                            self.chars = chars;
                            break;
                        }
                    }
                    Some((i, _)) => {
                        end = *i;
                        self.chars.next();
                    }
                    None => break,
                }
            }
            unsafe { self.source.get_unchecked(start..end) }
        };

        self.skip_ws();
        let binding = self.parse_svelte_binding()?;

        self.skip_ws();
        let index = if self.chars.next_if(|(_, c)| *c == ',').is_some() {
            self.skip_ws();
            Some(self.parse_identifier()?)
        } else {
            None
        };

        self.skip_ws();
        let key = if matches!(self.chars.peek(), Some((_, '('))) {
            Some(self.parse_inside('(', ')', false)?)
        } else {
            None
        };

        self.skip_ws();
        if self.chars.next_if(|(_, c)| *c == '}').is_none() {
            return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteEachBlock));
        }

        let children = self.parse_svelte_block_children()?;

        let else_children = if self
            .try_parse(|parser| {
                parser
                    .chars
                    .next_if(|(_, c)| *c == '{')
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == ':'))
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == 'e'))
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == 'l'))
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == 's'))
                    .and_then(|_| parser.chars.next_if(|(_, c)| *c == 'e'))
                    .and_then(|_| {
                        parser.skip_ws();
                        parser.chars.next_if(|(_, c)| *c == '}')
                    })
                    .ok_or_else(|| parser.emit_error(SyntaxErrorKind::ExpectSvelteEachBlock))
            })
            .is_ok()
        {
            Some(self.parse_svelte_block_children()?)
        } else {
            None
        };

        if self
            .chars
            .next_if(|(_, c)| *c == '{')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '/'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'e'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'a'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'c'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'h'))
            .map(|_| self.skip_ws())
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '}'))
            .is_some()
        {
            Ok(SvelteEachBlock {
                expr,
                binding,
                index,
                key,
                children,
                else_children,
            })
        } else {
            Err(self.emit_error(SyntaxErrorKind::ExpectSvelteBlockEnd))
        }
    }

    fn parse_svelte_if_block(&mut self) -> PResult<SvelteIfBlock<'s>> {
        if self
            .chars
            .next_if(|(_, c)| *c == '{')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '#'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'i'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'f'))
            .and_then(|_| self.chars.next_if(|(_, c)| c.is_ascii_whitespace()))
            .is_none()
        {
            return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteIfBlock));
        };

        let expr = self.parse_svelte_or_astro_expr()?;
        let children = self.parse_svelte_block_children()?;

        let mut else_if_blocks = vec![];
        let mut else_children = None;
        loop {
            if self.chars.next_if(|(_, c)| *c == '{').is_none() {
                return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteBlockEnd));
            }
            match self.chars.next() {
                Some((_, ':')) => {
                    if self
                        .chars
                        .next_if(|(_, c)| *c == 'e')
                        .and_then(|_| self.chars.next_if(|(_, c)| *c == 'l'))
                        .and_then(|_| self.chars.next_if(|(_, c)| *c == 's'))
                        .and_then(|_| self.chars.next_if(|(_, c)| *c == 'e'))
                        .is_none()
                    {
                        return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteElseIfBlock));
                    }
                    self.skip_ws();
                    match self.chars.next() {
                        Some((_, 'i')) => {
                            if self.chars.next_if(|(_, c)| *c == 'f').is_none() {
                                return Err(
                                    self.emit_error(SyntaxErrorKind::ExpectSvelteElseIfBlock)
                                );
                            }
                            let expr = self.parse_svelte_or_astro_expr()?;
                            let children = self.parse_svelte_block_children()?;
                            else_if_blocks.push(SvelteElseIfBlock { expr, children });
                        }
                        Some((_, '}')) => {
                            else_children = Some(self.parse_svelte_block_children()?);
                        }
                        _ => return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteElseIfBlock)),
                    }
                }
                Some((_, '/')) => break,
                _ => return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteBlockEnd)),
            }
        }
        if self
            .chars
            .next_if(|(_, c)| *c == 'i')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'f'))
            .map(|_| self.skip_ws())
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '}'))
            .is_some()
        {
            Ok(SvelteIfBlock {
                expr,
                children,
                else_if_blocks,
                else_children,
            })
        } else {
            Err(self.emit_error(SyntaxErrorKind::ExpectSvelteBlockEnd))
        }
    }

    fn parse_svelte_interpolation(&mut self) -> PResult<SvelteInterpolation<'s>> {
        if self.chars.next_if(|(_, c)| *c == '{').is_some() {
            Ok(SvelteInterpolation {
                expr: self.parse_svelte_or_astro_expr()?,
            })
        } else {
            Err(self.emit_error(SyntaxErrorKind::ExpectSvelteInterpolation))
        }
    }

    fn parse_svelte_key_block(&mut self) -> PResult<SvelteKeyBlock<'s>> {
        if self
            .chars
            .next_if(|(_, c)| *c == '{')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '#'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'k'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'e'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'y'))
            .and_then(|_| self.chars.next_if(|(_, c)| c.is_ascii_whitespace()))
            .is_none()
        {
            return Err(self.emit_error(SyntaxErrorKind::ExpectSvelteKeyBlock));
        };

        let expr = self.parse_svelte_or_astro_expr()?;
        let children = self.parse_svelte_block_children()?;

        if self
            .chars
            .next_if(|(_, c)| *c == '{')
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '/'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'k'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'e'))
            .and_then(|_| self.chars.next_if(|(_, c)| *c == 'y'))
            .map(|_| self.skip_ws())
            .and_then(|_| self.chars.next_if(|(_, c)| *c == '}'))
            .is_some()
        {
            Ok(SvelteKeyBlock { expr, children })
        } else {
            Err(self.emit_error(SyntaxErrorKind::ExpectSvelteBlockEnd))
        }
    }

    /// This will consume `}`.
    fn parse_svelte_or_astro_expr(&mut self) -> PResult<&'s str> {
        self.skip_ws();

        let start = self
            .chars
            .peek()
            .map(|(i, _)| *i)
            .unwrap_or(self.source.len());
        let mut end = start;
        let mut braces_stack = 0u8;
        loop {
            match self.chars.next() {
                Some((_, '{')) => {
                    braces_stack += 1;
                }
                Some((i, '}')) => {
                    if braces_stack == 0 {
                        end = i;
                        break;
                    }
                    braces_stack -= 1;
                }
                Some(..) => continue,
                None => break,
            }
        }
        Ok(unsafe { self.source.get_unchecked(start..end) })
    }

    fn parse_tag_name(&mut self) -> PResult<&'s str> {
        let start = match self.chars.peek() {
            Some((i, c)) if is_tag_name_char(*c) => {
                let start = *i;
                self.chars.next();
                start
            }
            Some((_, '>')) if matches!(self.language, Language::Astro) => {
                // Astro allows fragment
                return Ok("");
            }
            _ => return Err(self.emit_error(SyntaxErrorKind::ExpectTagName)),
        };
        let mut end = start;

        while let Some((i, _)) = self.chars.next_if(|(_, c)| is_tag_name_char(*c)) {
            end = i;
        }

        unsafe { Ok(self.source.get_unchecked(start..=end)) }
    }

    fn parse_text_node(&mut self) -> PResult<TextNode<'s>> {
        let Some((start, first_char)) = self.chars.next_if(|(_, c)| {
            if let Language::Vue | Language::Svelte = self.language {
                *c != '{'
            } else {
                true
            }
        }) else {
            return Err(self.emit_error(SyntaxErrorKind::ExpectTextNode));
        };

        if matches!(self.language, Language::Vue)
            && first_char == '{'
            && matches!(self.chars.peek(), Some((_, '{')))
        {
            return Err(self.emit_error(SyntaxErrorKind::ExpectTextNode));
        }

        let mut line_breaks = if first_char == '\n' { 1 } else { 0 };
        let end;
        loop {
            match self.chars.peek() {
                Some((i, '{')) => match self.language {
                    Language::Html => {
                        self.chars.next();
                    }
                    Language::Vue | Language::Vento => {
                        let i = *i;
                        let mut chars = self.chars.clone();
                        chars.next();
                        if chars.next_if(|(_, c)| *c == '{').is_some() {
                            end = i;
                            break;
                        }
                        self.chars.next();
                    }
                    Language::Svelte | Language::Astro => {
                        end = *i;
                        break;
                    }
                    Language::Jinja => {
                        let i = *i;
                        let mut chars = self.chars.clone();
                        chars.next();
                        if chars
                            .next_if(|(_, c)| *c == '%' || *c == '{' || *c == '#')
                            .is_some()
                        {
                            end = i;
                            break;
                        }
                        self.chars.next();
                    }
                },
                Some((i, '<')) => {
                    let i = *i;
                    let mut chars = self.chars.clone();
                    chars.next();
                    match chars.next() {
                        Some((_, c))
                            if is_tag_name_char(c)
                                || c == '/'
                                || c == '!'
                                || c == '>' && matches!(self.language, Language::Astro) =>
                        {
                            end = i;
                            break;
                        }
                        _ => {
                            self.chars.next();
                        }
                    }
                }
                Some((i, '-'))
                    if matches!(self.language, Language::Astro)
                        && !self.state.has_astro_front_matter =>
                {
                    let i = *i;
                    let mut chars = self.chars.clone();
                    chars.next();
                    if let Some(((_, '-'), (_, '-'))) = chars.next().zip(chars.next()) {
                        end = i;
                        break;
                    }
                    self.chars.next();
                }
                Some((_, c)) => {
                    if *c == '\n' {
                        line_breaks += 1;
                    }
                    self.chars.next();
                }
                None => {
                    end = self.source.len();
                    break;
                }
            }
        }

        Ok(TextNode {
            raw: unsafe { self.source.get_unchecked(start..end) },
            line_breaks,
        })
    }

    fn parse_vento_block_children(&mut self) -> PResult<Vec<Node<'s>>> {
        let mut children = vec![];
        loop {
            match self.chars.peek() {
                Some((_, '{')) => {
                    let mut chars = self.chars.clone();
                    chars.next();
                    if chars.next_if(|(_, c)| *c == '{').is_some() {
                        break;
                    }
                    children.push(self.parse_node()?);
                }
                Some(..) => {
                    children.push(self.parse_node()?);
                }
                None => return Err(self.emit_error(SyntaxErrorKind::ExpectVentoBlockEnd)),
            }
        }
        Ok(children)
    }

    fn parse_vento_tag_or_block(&mut self, first_tag: Option<&'s str>) -> PResult<Node<'s>> {
        let first_tag = if let Some(first_tag) = first_tag {
            first_tag
        } else {
            self.parse_mustache_interpolation()?
        };

        if let Some(raw) = first_tag
            .strip_prefix('#')
            .and_then(|s| s.strip_suffix('#'))
        {
            return Ok(Node::VentoComment(VentoComment { raw }));
        } else if let Some(raw) = first_tag.strip_prefix('>') {
            return Ok(Node::VentoEval(VentoEval { raw }));
        }

        let (tag_name, tag_rest) = helpers::parse_vento_tag(&first_tag);

        let is_function = tag_name == "function"
            || matches!(tag_name, "async" | "export") && tag_rest.starts_with("function");
        if matches!(tag_name, "for" | "if" | "layout")
            || matches!(tag_name, "set" | "export") && !first_tag.contains('=')
            || is_function
        {
            let mut body = vec![VentoTagOrChildren::Tag(VentoTag { tag: first_tag })];

            loop {
                let mut children = self.parse_vento_block_children()?;
                if !children.is_empty() {
                    if let Some(VentoTagOrChildren::Children(nodes)) = body.last_mut() {
                        nodes.append(&mut children);
                    } else {
                        body.push(VentoTagOrChildren::Children(children));
                    }
                }
                if let Ok(next_tag) = self.parse_mustache_interpolation() {
                    let (next_tag_name, _) = helpers::parse_vento_tag(&next_tag);
                    if next_tag_name
                        .trim()
                        .strip_prefix('/')
                        .is_some_and(|name| name == tag_name || is_function && name == "function")
                    {
                        body.push(VentoTagOrChildren::Tag(VentoTag { tag: next_tag }));
                        break;
                    }
                    if tag_name == "if" && next_tag_name == "else" {
                        body.push(VentoTagOrChildren::Tag(VentoTag { tag: next_tag }));
                    } else {
                        let node = self.parse_vento_tag_or_block(Some(next_tag))?;
                        if let Some(VentoTagOrChildren::Children(nodes)) = body.last_mut() {
                            nodes.push(node);
                        } else {
                            body.push(VentoTagOrChildren::Children(vec![node]));
                        }
                    }
                } else {
                    break;
                }
            }
            Ok(Node::VentoBlock(VentoBlock { body }))
        } else if is_vento_interpolation(tag_name) {
            Ok(Node::VentoInterpolation(VentoInterpolation {
                expr: first_tag,
            }))
        } else {
            Ok(Node::VentoTag(VentoTag { tag: first_tag }))
        }
    }

    fn parse_vue_directive(&mut self) -> PResult<VueDirective<'s>> {
        let name = match self.chars.peek() {
            Some((_, ':')) => {
                self.chars.next();
                ":"
            }
            Some((_, '@')) => {
                self.chars.next();
                "@"
            }
            Some((_, '#')) => {
                self.chars.next();
                "#"
            }
            Some((_, 'v')) => {
                let mut chars = self.chars.clone();
                chars.next();
                if chars.next_if(|(_, c)| *c == '-').is_some() {
                    self.chars = chars;
                    self.parse_identifier()?
                } else {
                    return Err(self.emit_error(SyntaxErrorKind::ExpectVueDirective));
                }
            }
            _ => return Err(self.emit_error(SyntaxErrorKind::ExpectVueDirective)),
        };

        let arg_and_modifiers = if matches!(name, ":" | "@" | "#")
            || self
                .chars
                .peek()
                .map(|(_, c)| is_attr_name_char(*c))
                .unwrap_or_default()
        {
            Some(self.parse_attr_name()?)
        } else {
            None
        };

        self.skip_ws();
        let value = if self.chars.next_if(|(_, c)| *c == '=').is_some() {
            self.skip_ws();
            Some(self.parse_attr_value()?)
        } else {
            None
        };

        Ok(VueDirective {
            name,
            arg_and_modifiers,
            value,
        })
    }
}

fn is_tag_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || c == '-'
        || c == '_'
        || c == '.'
        || c == ':'
        || !c.is_ascii()
        || c == '\\'
}

fn is_attr_name_char(c: char) -> bool {
    !matches!(c, '"' | '\'' | '>' | '/' | '=') && !c.is_ascii_whitespace()
}

fn parse_jinja_tag_name<'s>(tag: &JinjaTag<'s>) -> &'s str {
    let trimmed = tag.content.trim_start_matches(['+', '-']).trim_start();
    trimmed
        .split_once(|c: char| c.is_ascii_whitespace())
        .map(|(name, _)| name)
        .unwrap_or(trimmed)
}

fn is_vento_interpolation(tag_name: &str) -> bool {
    !matches!(
        tag_name,
        "if" | "else"
            | "for"
            | "set"
            | "include"
            | "layout"
            | "async"
            | "function"
            | "import"
            | "export"
    )
}

pub type PResult<T> = Result<T, SyntaxError>;
