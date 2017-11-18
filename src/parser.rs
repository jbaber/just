use common::*;

use itertools;
use TokenKind::*;
use CompilationErrorKind::*;
use recipe_resolver::resolve_recipes;
use assignment_resolver::resolve_assignments;

pub struct Parser<'a> {
  text:              &'a str,
  tokens:            itertools::PutBack<vec::IntoIter<Token<'a>>>,
  recipes:           Map<&'a str, Recipe<'a>>,
  assignments:       Map<&'a str, Expression<'a>>,
  assignment_tokens: Map<&'a str, Token<'a>>,
  exports:           Set<&'a str>,
}

impl<'a> Parser<'a> {
  pub fn new(text: &'a str, tokens: Vec<Token<'a>>) -> Parser<'a> {
    Parser {
      text:              text,
      tokens:            itertools::put_back(tokens),
      recipes:           empty(),
      assignments:       empty(),
      assignment_tokens: empty(),
      exports:           empty(),
    }
  }

  fn peek(&mut self, kind: TokenKind) -> bool {
    let next = self.tokens.next().unwrap();
    let result = next.kind == kind;
    self.tokens.put_back(next);
    result
  }

  fn accept(&mut self, kind: TokenKind) -> Option<Token<'a>> {
    if self.peek(kind) {
      self.tokens.next()
    } else {
      None
    }
  }

  fn accept_any(&mut self, kinds: &[TokenKind]) -> Option<Token<'a>> {
    for kind in kinds {
      if self.peek(*kind) {
        return self.tokens.next();
      }
    }
    None
  }

  fn accepted(&mut self, kind: TokenKind) -> bool {
    self.accept(kind).is_some()
  }

  fn expect(&mut self, kind: TokenKind) -> Option<Token<'a>> {
    if self.peek(kind) {
      self.tokens.next();
      None
    } else {
      self.tokens.next()
    }
  }

  fn expect_eol(&mut self) -> Option<Token<'a>> {
    self.accepted(Comment);
    if self.peek(Eol) {
      self.accept(Eol);
      None
    } else if self.peek(Eof) {
      None
    } else {
      self.tokens.next()
    }
  }

  fn unexpected_token(&self, found: &Token<'a>, expected: &[TokenKind]) -> CompilationError<'a> {
    found.error(UnexpectedToken {
      expected: expected.to_vec(),
      found:    found.kind,
    })
  }

  fn recipe(
    &mut self,
    name:  &Token<'a>,
    doc:   Option<Token<'a>>,
    quiet: bool,
  ) -> CompilationResult<'a, ()> {
    if let Some(recipe) = self.recipes.get(name.lexeme) {
      return Err(name.error(DuplicateRecipe {
        recipe: recipe.name,
        first:  recipe.line_number
      }));
    }

    let mut parsed_parameter_with_default = false;
    let mut parsed_variadic_parameter = false;
    let mut parameters: Vec<Parameter> = vec![];
    loop {
      let plus = self.accept(Plus);

      let parameter = match self.accept(Name) {
        Some(parameter) => parameter,
        None            => if let Some(plus) = plus {
          return Err(self.unexpected_token(&plus, &[Name]));
        } else {
          break
        },
      };

      let variadic = plus.is_some();

      if parsed_variadic_parameter {
        return Err(parameter.error(ParameterFollowsVariadicParameter {
          parameter: parameter.lexeme,
        }));
      }

      if parameters.iter().any(|p| p.name == parameter.lexeme) {
        return Err(parameter.error(DuplicateParameter {
          recipe: name.lexeme, parameter: parameter.lexeme
        }));
      }

      let default;
      if self.accepted(Equals) {
        if let Some(string) = self.accept_any(&[StringToken, RawString]) {
          default = Some(CookedString::new(&string)?.cooked);
        } else {
          let unexpected = self.tokens.next().unwrap();
          return Err(self.unexpected_token(&unexpected, &[StringToken, RawString]));
        }
      } else {
        default = None
      }

      if parsed_parameter_with_default && default.is_none() {
        return Err(parameter.error(RequiredParameterFollowsDefaultParameter{
          parameter: parameter.lexeme,
        }));
      }

      parsed_parameter_with_default |= default.is_some();
      parsed_variadic_parameter = variadic;

      parameters.push(Parameter {
        default:  default,
        name:     parameter.lexeme,
        token:    parameter,
        variadic: variadic,
      });
    }

    if let Some(token) = self.expect(Colon) {
      // if we haven't accepted any parameters, an equals
      // would have been fine as part of an assignment
      if parameters.is_empty() {
        return Err(self.unexpected_token(&token, &[Name, Plus, Colon, Equals]));
      } else {
        return Err(self.unexpected_token(&token, &[Name, Plus, Colon]));
      }
    }

    let mut dependencies = vec![];
    let mut dependency_tokens = vec![];
    while let Some(dependency) = self.accept(Name) {
      if dependencies.contains(&dependency.lexeme) {
        return Err(dependency.error(DuplicateDependency {
          recipe:     name.lexeme,
          dependency: dependency.lexeme
        }));
      }
      dependencies.push(dependency.lexeme);
      dependency_tokens.push(dependency);
    }

    if let Some(token) = self.expect_eol() {
      return Err(self.unexpected_token(&token, &[Name, Eol, Eof]));
    }

    let mut lines: Vec<Vec<Fragment>> = vec![];
    let mut shebang = false;

    if self.accepted(Indent) {
      while !self.accepted(Dedent) {
        if self.accepted(Eol) {
          lines.push(vec![]);
          continue;
        }
        if let Some(token) = self.expect(Line) {
          return Err(token.error(Internal{
            message: format!("Expected a line but got {}", token.kind)
          }))
        }
        let mut fragments = vec![];

        while !(self.accepted(Eol) || self.peek(Dedent)) {
          if let Some(token) = self.accept(Text) {
            if fragments.is_empty() {
              if lines.is_empty() {
                if token.lexeme.starts_with("#!") {
                  shebang = true;
                }
              } else if !shebang
                && !lines.last().and_then(|line| line.last())
                  .map(Fragment::continuation).unwrap_or(false)
                && (token.lexeme.starts_with(' ') || token.lexeme.starts_with('\t')) {
                return Err(token.error(ExtraLeadingWhitespace));
              }
            }
            fragments.push(Fragment::Text{text: token});
          } else if let Some(token) = self.expect(InterpolationStart) {
            return Err(self.unexpected_token(&token, &[Text, InterpolationStart, Eol]));
          } else {
            fragments.push(Fragment::Expression{
              expression: self.expression(true)?
            });
            if let Some(token) = self.expect(InterpolationEnd) {
              return Err(self.unexpected_token(&token, &[InterpolationEnd]));
            }
          }
        }

        lines.push(fragments);
      }
    }

    self.recipes.insert(name.lexeme, Recipe {
      line_number:       name.line,
      name:              name.lexeme,
      doc:               doc.map(|t| t.lexeme[1..].trim()),
      dependencies:      dependencies,
      dependency_tokens: dependency_tokens,
      parameters:        parameters,
      private:           &name.lexeme[0..1] == "_",
      lines:             lines,
      shebang:           shebang,
      quiet:             quiet,
    });

    Ok(())
  }

  fn expression(&mut self, interpolation: bool) -> CompilationResult<'a, Expression<'a>> {
    let first = self.tokens.next().unwrap();
    let lhs = match first.kind {
      Name        => Expression::Variable {name: first.lexeme, token: first},
      Backtick    => Expression::Backtick {
        raw:   &first.lexeme[1..first.lexeme.len()-1],
        token: first
      },
      RawString | StringToken => {
        Expression::String{cooked_string: CookedString::new(&first)?}
      }
      _ => return Err(self.unexpected_token(&first, &[Name, StringToken])),
    };

    if self.accepted(Plus) {
      let rhs = self.expression(interpolation)?;
      Ok(Expression::Concatination{lhs: Box::new(lhs), rhs: Box::new(rhs)})
    } else if interpolation && self.peek(InterpolationEnd) {
      Ok(lhs)
    } else if let Some(token) = self.expect_eol() {
      if interpolation {
        return Err(self.unexpected_token(&token, &[Plus, Eol, InterpolationEnd]))
      } else {
        Err(self.unexpected_token(&token, &[Plus, Eol]))
      }
    } else {
      Ok(lhs)
    }
  }

  fn assignment(&mut self, name: Token<'a>, export: bool) -> CompilationResult<'a, ()> {
    if self.assignments.contains_key(name.lexeme) {
      return Err(name.error(DuplicateVariable {variable: name.lexeme}));
    }
    if export {
      self.exports.insert(name.lexeme);
    }
    let expression = self.expression(false)?;
    self.assignments.insert(name.lexeme, expression);
    self.assignment_tokens.insert(name.lexeme, name);
    Ok(())
  }

  pub fn justfile(mut self) -> CompilationResult<'a, Justfile<'a>> {
    let mut doc = None;
    loop {
      match self.tokens.next() {
        Some(token) => match token.kind {
          Eof => break,
          Eol => {
            doc = None;
            continue;
          }
          Comment => {
            if let Some(token) = self.expect_eol() {
              return Err(token.error(Internal {
                message: format!("found comment followed by {}", token.kind),
              }));
            }
            doc = Some(token);
          }
          At => if let Some(name) = self.accept(Name) {
            self.recipe(&name, doc, true)?;
            doc = None;
          } else {
            let unexpected = &self.tokens.next().unwrap();
            return Err(self.unexpected_token(unexpected, &[Name]));
          },
          Name => if token.lexeme == "export" {
            let next = self.tokens.next().unwrap();
            if next.kind == Name && self.accepted(Equals) {
              self.assignment(next, true)?;
              doc = None;
            } else {
              self.tokens.put_back(next);
              self.recipe(&token, doc, false)?;
              doc = None;
            }
          } else if self.accepted(Equals) {
            self.assignment(token, false)?;
            doc = None;
          } else {
            self.recipe(&token, doc, false)?;
            doc = None;
          },
          _ => return Err(self.unexpected_token(&token, &[Name, At])),
        },
        None => return Err(CompilationError {
          text:   self.text,
          index:  0,
          line:   0,
          column: 0,
          width:  None,
          kind:   Internal {
            message: "unexpected end of token stream".to_string()
          }
        }),
      }
    }

    if let Some(token) = self.tokens.next() {
      return Err(token.error(Internal {
        message: format!("unexpected token remaining after parsing completed: {:?}", token.kind)
      }))
    }

    resolve_recipes(&self.recipes, &self.assignments, self.text)?;

    for recipe in self.recipes.values() {
      for parameter in &recipe.parameters {
        if self.assignments.contains_key(parameter.token.lexeme) {
          return Err(parameter.token.error(ParameterShadowsVariable {
            parameter: parameter.token.lexeme
          }));
        }
      }

      for dependency in &recipe.dependency_tokens {
        if !self.recipes[dependency.lexeme].parameters.is_empty() {
          return Err(dependency.error(DependencyHasParameters {
            recipe: recipe.name,
            dependency: dependency.lexeme,
          }));
        }
      }
    }

    resolve_assignments(&self.assignments, &self.assignment_tokens)?;

    Ok(Justfile {
      recipes:     self.recipes,
      assignments: self.assignments,
      exports:     self.exports,
    })
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use brev;
  use testing::parse_success;
  use testing::parse_error;

  macro_rules! summary_test {
    ($name:ident, $input:expr, $expected:expr $(,)*) => {
      #[test]
      fn $name() {
        let input = $input;
        let expected = $expected;
        let justfile = parse_success(input);
        let actual = format!("{:#}", justfile);
        if actual != expected {
          println!("got:\n\"{}\"\n", actual);
          println!("\texpected:\n\"{}\"", expected);
          assert_eq!(actual, expected);
        }
      }
    }
  }

  summary_test!{parse_empty,
    "

# hello


    ", 
    "",
  }

  summary_test!{parse_string_default,
    r#"

foo a="b\t":


  "#,
    r#"foo a='b\t':"#,
  }

  summary_test!{parse_variadic,
    r#"

foo +a:


  "#,
    r#"foo +a:"#,
  }

  summary_test!{parse_variadic_string_default,
    r#"

foo +a="Hello":


  "#, 
    r#"foo +a='Hello':"#,
  }

  summary_test!{parse_raw_string_default,
    r#"

foo a='b\t':


  "#, 
    r#"foo a='b\\t':"#,
  }

  summary_test!{parse_export,
    r#"
export a = "hello"

  "#, 
    r#"export a = "hello""#,
  }

  summary_test!{parse_complex,
    "
x:
y:
z:
foo = \"xx\"
bar = foo
goodbye = \"y\"
hello a b    c   : x y    z #hello
  #! blah
  #blarg
  {{ foo + bar}}abc{{ goodbye\t  + \"x\" }}xyz
  1
  2
  3
", 
    "bar = foo

foo = \"xx\"

goodbye = \"y\"

hello a b c: x y z
    #! blah
    #blarg
    {{foo + bar}}abc{{goodbye + \"x\"}}xyz
    1
    2
    3

x:

y:

z:"
  }

  summary_test!{parse_shebang,
    "
practicum = 'hello'
install:
\t#!/bin/sh
\tif [[ -f {{practicum}} ]]; then
\t\treturn
\tfi
", 
    "practicum = \"hello\"

install:
    #!/bin/sh
    if [[ -f {{practicum}} ]]; then
    \treturn
    fi",
  }

  summary_test!{parse_assignments,
    r#"a = "0"
c = a + b + a + b
b = "1"
"#,
    r#"a = "0"

b = "1"

c = a + b + a + b"#,
  }

  summary_test!{parse_assignment_backticks,
    "a = `echo hello`
c = a + b + a + b
b = `echo goodbye`",
    "a = `echo hello`

b = `echo goodbye`

c = a + b + a + b",
  }

  summary_test!{parse_interpolation_backticks,
    r#"a:
 echo {{  `echo hello` + "blarg"   }} {{   `echo bob`   }}"#,
    r#"a:
    echo {{`echo hello` + "blarg"}} {{`echo bob`}}"#,
  }

  summary_test!{eof_test,
    "x:\ny:\nz:\na b c: x y z",
    "a b c: x y z\n\nx:\n\ny:\n\nz:",
  }

  summary_test!{string_quote_escape,
    r#"a = "hello\"""#,
    r#"a = "hello\"""#,
  }

  summary_test!{string_escapes,
    r#"a = "\n\t\r\"\\""#,
    r#"a = "\n\t\r\"\\""#,
  }

  summary_test!{parameters,
    "a b c:
  {{b}} {{c}}",
    "a b c:
    {{b}} {{c}}",
  }

  #[test]
  fn missing_colon() {
    let text = "a b c\nd e f";
    parse_error(text, CompilationError {
      text:   text,
      index:  5,
      line:   0,
      column: 5,
      width:  Some(1),
      kind:   UnexpectedToken{expected: vec![Name, Plus, Colon], found: Eol},
    });
  }

  #[test]
  fn missing_default_eol() {
    let text     = "hello arg=\n";
    let expected = vec![StringToken, RawString];
    let found    = Eol;
    parse_error(text, CompilationError {
      text:   text,
      index:  10,
      line:   0,
      column: 10,
      width:  Some(1),
      kind:   UnexpectedToken{expected, found},
    });
  }

  #[test]
  fn missing_default_eof() {
    let text     = "hello arg=";
    let expected = vec![StringToken, RawString];
    let found    = Eof;
    parse_error(text, CompilationError {
      text:   text,
      index:  10,
      line:   0,
      column: 10,
      width:  Some(0),
      kind:   UnexpectedToken{expected, found},
    });
  }

  #[test]
  fn missing_default_colon() {
    let text     = "hello arg=:";
    let expected = vec![StringToken, RawString];
    let found    = Colon;
    parse_error(text, CompilationError {
      text:   text,
      index:  10,
      line:   0,
      column: 10,
      width:  Some(1),
      kind:   UnexpectedToken{expected, found},
    });
  }

  #[test]
  fn missing_default_backtick() {
    let text     = "hello arg=`hello`";
    let expected = vec![StringToken, RawString];
    let found    = Backtick;
    parse_error(text, CompilationError {
      text:   text,
      index:  10,
      line:   0,
      column: 10,
      width:  Some(7),
      kind:   UnexpectedToken{expected, found},
    });
  }

  #[test]
  fn parameter_after_variadic() {
    let text = "foo +a bbb:";
    parse_error(text, CompilationError {
      text:   text,
      index:  7,
      line:   0,
      column: 7,
      width:  Some(3),
      kind:   ParameterFollowsVariadicParameter{parameter: "bbb"}
    });
  }

  #[test]
  fn required_after_default() {
    let text = "hello arg='foo' bar:";
    parse_error(text, CompilationError {
      text:   text,
      index:  16,
      line:   0,
      column: 16,
      width:  Some(3),
      kind:   RequiredParameterFollowsDefaultParameter{parameter: "bar"},
    });
  }

  #[test]
  fn missing_eol() {
    let text = "a b c: z =";
    parse_error(text, CompilationError {
      text:   text,
      index:  9,
      line:   0,
      column: 9,
      width:  Some(1),
      kind:   UnexpectedToken{expected: vec![Name, Eol, Eof], found: Equals},
    });
  }

  #[test]
  fn duplicate_parameter() {
    let text = "a b b:";
    parse_error(text, CompilationError {
      text:   text,
      index:  4,
      line:   0,
      column: 4,
      width:  Some(1),
      kind:   DuplicateParameter{recipe: "a", parameter: "b"}
    });
  }

  #[test]
  fn parameter_shadows_varible() {
    let text = "foo = \"h\"\na foo:";
    parse_error(text, CompilationError {
      text:   text,
      index:  12,
      line:   1,
      column: 2,
      width:  Some(3),
      kind:   ParameterShadowsVariable{parameter: "foo"}
    });
  }

  #[test]
  fn dependency_has_parameters() {
    let text = "foo arg:\nb: foo";
    parse_error(text, CompilationError {
      text:   text,
      index:  12,
      line:   1,
      column: 3,
      width:  Some(3),
      kind:   DependencyHasParameters{recipe: "b", dependency: "foo"}
    });
  }

  #[test]
  fn duplicate_dependency() {
    let text = "a b c: b c z z";
    parse_error(text, CompilationError {
      text:   text,
      index:  13,
      line:   0,
      column: 13,
      width:  Some(1),
      kind:   DuplicateDependency{recipe: "a", dependency: "z"}
    });
  }

  #[test]
  fn duplicate_recipe() {
    let text = "a:\nb:\na:";
    parse_error(text, CompilationError {
      text:   text,
      index:  6,
      line:   2,
      column: 0,
      width:  Some(1),
      kind:   DuplicateRecipe{recipe: "a", first: 0}
    });
  }

  #[test]
  fn duplicate_variable() {
    let text = "a = \"0\"\na = \"0\"";
    parse_error(text, CompilationError {
      text:   text,
      index:  8,
      line:   1,
      column: 0,
      width:  Some(1),
      kind:   DuplicateVariable{variable: "a"}
    });
  }

  #[test]
  fn extra_whitespace() {
    let text = "a:\n blah\n  blarg";
    parse_error(text, CompilationError {
      text:   text,
      index:  10,
      line:   2,
      column: 1,
      width:  Some(6),
      kind:   ExtraLeadingWhitespace
    });
    // extra whitespace is okay in a shebang recipe
    parse_success("a:\n #!\n  print(1)");
  }

  #[test]
  fn interpolation_outside_of_recipe() {
    let text     = "{{";
    let expected = vec![Name, At];
    let found    = InterpolationStart;
    parse_error(text, CompilationError {
      text:   text,
      index:  0,
      line:   0,
      column: 0,
      width:  Some(2),
      kind:   UnexpectedToken{expected, found},
    });
  }

  #[test]
  fn unclosed_interpolation_delimiter() {
    let text     = "a:\n echo {{ foo";
    let expected = vec![Plus, Eol, InterpolationEnd];
    let found    = Dedent;
    parse_error(text, CompilationError {
      text:   text,
      index:  15,
      line:   1,
      column: 12,
      width:  Some(0),
      kind:   UnexpectedToken{expected, found},
    });
  }

  #[test]
  fn plus_following_parameter() {
    let text = "a b c+:";
    parse_error(text, CompilationError {
      text:   text,
      index:  5,
      line:   0,
      column: 5,
      width:  Some(1),
      kind:   UnexpectedToken{expected: vec![Name], found: Plus},
    });
  }

  #[test]
  fn readme_test() {
    let mut justfiles = vec![];
    let mut current = None;

    for line in brev::slurp("README.asc").lines() {
      if let Some(mut justfile) = current {
        if line == "```" {
          justfiles.push(justfile);
          current = None;
        } else {
          justfile += line;
          justfile += "\n";
          current = Some(justfile);
        }
      } else if line == "```make" {
        current = Some(String::new());
      }
    }

    for justfile in justfiles {
      parse_success(&justfile);
    }
  }
}