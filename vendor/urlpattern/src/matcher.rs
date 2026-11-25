use crate::Error;
use crate::regexp::RegExp;

#[derive(Debug)]
/// A structured representation of a URLPattern matcher, which can be used to
/// match a URL against a pattern quickly.
pub struct Matcher<R: RegExp> {
  pub prefix: String,
  pub suffix: String,
  pub inner: InnerMatcher<R>,
  pub ignore_case: bool,
}

#[derive(Debug)]
pub enum InnerMatcher<R: RegExp> {
  /// A literal string matcher.
  ///
  /// # Examples
  /// - /
  /// - /foo
  Literal { literal: String },
  /// A matcher that matches all chars, except the substring specified in
  /// `filter` (if it is set).
  ///
  /// # Examples
  /// - *
  /// - /old/*
  /// - /scripts/*.js
  /// - /:slug
  /// - /blog/:id
  /// - /blog/:id.html
  SingleCapture {
    filter: Option<char>,
    allow_empty: bool,
  },
  /// A regexp matcher. This is a bail-out matcher for arbitrary complexity
  /// matchers.
  ///
  /// # Examples
  /// - /foo/:id?
  RegExp { regexp: Result<R, Error> },
}

impl<R: RegExp> Matcher<R> {
  pub fn matches<'a>(
    &self,
    mut input: &'a str,
  ) -> Option<Vec<Option<&'a str>>> {
    let prefix_len = self.prefix.len();
    let suffix_len = self.suffix.len();
    let input_len = input.len();
    if prefix_len + suffix_len > 0 {
      // The input must be at least as long as the prefix and suffix combined,
      // because these must both be present, and not overlap.
      if input_len < prefix_len + suffix_len {
        return None;
      }
      // (Snowlark: the literal prefix and suffix compare case-insensitively under `ignoreCase`, as the
      // expression the whole component stands for does.)
      let equal = |a: &str, b: &str| {
        if self.ignore_case {
          a.to_lowercase() == b.to_lowercase()
        } else {
          a == b
        }
      };
      if !input.get(..prefix_len).is_some_and(|head| equal(head, &self.prefix))
      {
        return None;
      }
      if !input
        .get(input_len - suffix_len..)
        .is_some_and(|tail| equal(tail, &self.suffix))
      {
        return None;
      }
      input = &input[prefix_len..input_len - suffix_len];
    }

    match &self.inner {
      InnerMatcher::Literal { literal } => {
        if self.ignore_case {
          (input.to_lowercase() == literal.to_lowercase()).then(Vec::new)
        } else {
          (input == literal).then(Vec::new)
        }
      }
      InnerMatcher::SingleCapture {
        filter,
        allow_empty,
      } => {
        if input.is_empty() && !allow_empty {
          return None;
        }
        if let Some(filter) = filter {
          if self.ignore_case {
            if input
              .to_lowercase()
              .contains(filter.to_lowercase().collect::<Vec<_>>().as_slice())
            {
              return None;
            }
          } else if input.contains(*filter) {
            return None;
          }
        }
        Some(vec![Some(input)])
      }
      InnerMatcher::RegExp { regexp, .. } => {
        regexp.as_ref().ok()?.matches(input)
      }
    }
  }
}
