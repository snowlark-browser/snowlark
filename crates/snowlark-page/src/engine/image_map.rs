// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §4.8.14 image maps: the `area` element of an `img` element's image map at a point of the
//! image, which pointer input targets instead of the image (§4.8.14.2 "processing model").

use kurbo::{BezPath, Shape};
use snowlark_dom::{local_name, ns, Dom, NodeId};
use snowlark_paint::Point;

/// The `area` element of `img`'s image map whose shape contains `p` (CSS px from the top left of the
/// image's content box), the first in tree order when shapes overlap.
pub(crate) fn area_at(dom: &Dom, img: NodeId, p: Point) -> Option<NodeId> {
    if !dom.is_html_element_named(img, &local_name!("img")) {
        return None;
    }
    let usemap = dom.attr(img, &ns!(), &local_name!("usemap"))?;
    let map = map_for(dom, img, usemap)?;
    let point = kurbo::Point::new(p.x as f64, p.y as f64);
    dom.descendants(map)
        .filter(|&a| dom.is_html_element_named(a, &local_name!("area")))
        .find(|&a| area_contains(dom, a, point))
}

/// The rules for parsing a hash-name reference to a `map` element: the first `map` element in the
/// image's tree whose `id` or `name` is the text after the first `#`.
fn map_for(dom: &Dom, img: NodeId, reference: &str) -> Option<NodeId> {
    let (_, name) = reference.split_once('#')?;
    if name.is_empty() {
        return None;
    }
    let root = dom.root(img);
    dom.descendants(root).find(|&m| {
        dom.is_html_element_named(m, &local_name!("map"))
            && (dom.attr(m, &ns!(), &local_name!("id")) == Some(name)
                || dom.attr(m, &ns!(), &local_name!("name")) == Some(name))
    })
}

/// Whether `p` is inside the shape of `area` (its `shape` and `coords` attributes).
fn area_contains(dom: &Dom, area: NodeId, p: kurbo::Point) -> bool {
    let shape = dom
        .attr(area, &ns!(), &local_name!("shape"))
        .map(|s| s.to_ascii_lowercase());
    let coords = dom
        .attr(area, &ns!(), &local_name!("coords"))
        .map(float_list)
        .unwrap_or_default();
    match shape.as_deref() {
        Some("default") => true,
        Some("circle") | Some("circ") => {
            if coords.len() < 3 || coords[2] <= 0.0 {
                return false;
            }
            kurbo::Circle::new((coords[0], coords[1]), coords[2]).contains(p)
        }
        Some("poly") | Some("polygon") => {
            if coords.len() < 6 {
                return false;
            }
            let mut path = BezPath::new();
            for (i, &[x, y]) in coords.as_chunks::<2>().0.iter().enumerate() {
                if i == 0 {
                    path.move_to((x, y));
                } else {
                    path.line_to((x, y));
                }
            }
            path.close_path();
            path.contains(p)
        }
        // (the rectangle state: the missing value default and the invalid value default)
        _ => {
            if coords.len() < 4 {
                return false;
            }
            let (x0, x1) = (coords[0].min(coords[2]), coords[0].max(coords[2]));
            let (y0, y1) = (coords[1].min(coords[3]), coords[1].max(coords[3]));
            kurbo::Rect::new(x0, y0, x1, y1).contains(p)
        }
    }
}

/// HTML §2.3.4.3 the rules for parsing a list of floating-point numbers: tokens separated by ASCII
/// whitespace, commas and semicolons, each parsed as a floating-point number value (0 when it is
/// not one).
fn float_list(input: &str) -> Vec<f64> {
    let sep = |c: char| c.is_ascii_whitespace() || c == ',' || c == ';';
    input
        .split(sep)
        .filter(|t| !t.is_empty())
        .map(|t| float_prefix(t).unwrap_or(0.0))
        .collect()
}

/// HTML §2.3.4.3 the rules for parsing floating-point number values: the number at the start of
/// `s` (what follows it is ignored), or `None` when it does not start with one.
fn float_prefix(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut sign = 1.0;
    if b.first() == Some(&b'-') {
        sign = -1.0;
        i = 1;
    } else if b.first() == Some(&b'+') {
        i = 1;
    }
    let digits = |from: usize| b[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    let int = digits(i);
    let mut end = i + int;
    let mut frac = 0;
    if b.get(end) == Some(&b'.') {
        frac = digits(end + 1);
        if frac > 0 {
            end += 1 + frac;
        }
    }
    if int == 0 && frac == 0 {
        return None;
    }
    if matches!(b.get(end), Some(b'e') | Some(b'E')) {
        let mut j = end + 1;
        if matches!(b.get(j), Some(b'-') | Some(b'+')) {
            j += 1;
        }
        let exp = digits(j);
        if exp > 0 {
            end = j + exp;
        }
    }
    s[i..end].parse::<f64>().ok().map(|v| sign * v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinate_lists_parse_as_html_says() {
        assert_eq!(float_list("0,0, 99 ,50"), vec![0.0, 0.0, 99.0, 50.0]);
        assert_eq!(float_list("10px;20.5;x;-3e1"), vec![10.0, 20.5, 0.0, -30.0]);
        assert_eq!(float_list(" .5,+2, 7."), vec![0.5, 2.0, 7.0]);
        assert!(float_list("").is_empty());
    }
}
