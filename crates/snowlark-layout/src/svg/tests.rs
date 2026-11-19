// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::*;
use kurbo::Point;

#[test]
fn numbers_lists_and_points() {
    assert_eq!(parse::parse_number(" 1.5e2 "), Some(150.0));
    assert_eq!(parse::parse_number("1."), None);
    assert_eq!(
        parse_number_list("1, 2 3,4"),
        Some(vec![1.0, 2.0, 3.0, 4.0])
    );
    assert_eq!(parse_number_list("1,"), None);
    assert_eq!(parse_number_list(""), Some(vec![]));
    assert_eq!(parse_points("1,2 3,4"), Some(vec![(1.0, 2.0), (3.0, 4.0)]));
    assert_eq!(parse_points("1,2 3,"), Some(vec![(1.0, 2.0)]));
    assert_eq!(parse_points("1,2 x"), None);
    assert_eq!(fmt_number(2.5), "2.5");
    assert_eq!(fmt_number(-0.0), "0");
    assert_eq!(fmt_number(100.0), "100");
}

#[test]
fn lengths_parse_serialize_and_convert() {
    let l = Length::parse(" 10PX ").unwrap();
    assert_eq!(l.unit, LengthUnit::Px);
    assert_eq!(l.to_css(), "10px");
    assert_eq!(Length::parse("5%").unwrap().unit_type(), 2);
    assert_eq!(Length::parse("2rem").unwrap().unit_type(), 0);
    assert!(Length::parse("10 px").is_none());
    assert!(Length::parse("px").is_none());
    let ctx = LengthContext {
        viewport: Some((200.0, 100.0)),
        font_size: Some(10.0),
        ..Default::default()
    };
    assert_eq!(
        Length::parse("50%")
            .unwrap()
            .to_user(LengthDir::Horizontal, &ctx),
        100.0
    );
    assert_eq!(
        Length::parse("2em")
            .unwrap()
            .to_user(LengthDir::Vertical, &ctx),
        20.0
    );
    assert_eq!(
        Length::parse("1in")
            .unwrap()
            .to_user(LengthDir::Other, &ctx),
        96.0
    );
    let c = Length::converted(LengthUnit::Percent, 50.0, LengthDir::Vertical, &ctx);
    assert_eq!(c.to_css(), "50%");
    let a = Angle::parse("0.5turn").unwrap();
    assert_eq!(a.degrees(), 180.0);
    assert_eq!(Angle::converted(AngleUnit::Grad, 90.0).to_css(), "100grad");
}

#[test]
fn transform_lists() {
    let l = transform::parse_transform_list("translate(10) rotate(90, 5 5),scale(2)").unwrap();
    assert_eq!(l.len(), 3);
    assert_eq!(l[0].to_css(), "translate(10 0)");
    assert_eq!(l[1].to_css(), "rotate(90 5 5)");
    assert!(transform::parse_transform_list("translate(10,)").is_none());
    assert!(transform::parse_transform_list("scale(1) ,").is_none());
    assert_eq!(transform::parse_transform_list("none"), Some(vec![]));
    let m = transform::list_matrix(&l);
    let p = m * Point::new(1.0, 0.0);
    assert!(
        (p.x - 20.0).abs() < 1e-9 && (p.y - 2.0).abs() < 1e-9,
        "{p:?}"
    );
}

#[test]
fn path_data_error_handling() {
    let d = PathData::parse("M 10,10 L 20,20,30");
    assert!(d.error);
    assert_eq!(d.commands.len(), 2);
    let d = PathData::parse("M100-200");
    assert_eq!(d.commands[0].params(), &[100.0, -200.0]);
    let d = PathData::parse("M0.6.5");
    assert_eq!(d.commands[0].params(), &[0.6, 0.5]);
    assert!(PathData::parse("L 1 2").error);
    let d = PathData::parse("M0 0 a10 10 0 1110 10");
    assert!(!d.error, "{d:?}");
    assert_eq!(
        d.commands[1].params(),
        &[10.0, 10.0, 0.0, 1.0, 1.0, 10.0, 10.0]
    );
    assert!(PathData::parse("M 0 0 Z 1 2").error);
}

#[test]
fn path_measurement() {
    let p = PathData::parse("M0 0 H100 V100").to_bezpath();
    assert!((geom::total_length(&p) - 200.0).abs() < 1e-6);
    let q = geom::point_at_length(&p, 150.0);
    assert!((q.x - 100.0).abs() < 1e-6 && (q.y - 50.0).abs() < 1e-6);
    let q = geom::point_at_length(&p, 1000.0);
    assert!((q.y - 100.0).abs() < 1e-6);
    let circle = PathData::parse("M10 0 A10 10 0 0 1 -10 0 A10 10 0 0 1 10 0").to_bezpath();
    let len = geom::total_length(&circle);
    assert!(
        (len - 2.0 * std::f64::consts::PI * 10.0).abs() < 0.01,
        "{len}"
    );
    assert!(geom::contains_fill(&p, Point::new(90.0, 10.0), false));
    assert!(!geom::contains_fill(&p, Point::new(10.0, 90.0), false));
    assert!(geom::contains_fill(&p, Point::new(0.0, 0.0), false));
    let s = StrokeStyle {
        width: 10.0,
        ..Default::default()
    };
    assert!(geom::contains_stroke(&p, Point::new(50.0, 4.0), &s));
    assert!(!geom::contains_stroke(&p, Point::new(50.0, 6.0), &s));
    let b = geom::bounds(&circle, Matrix::IDENTITY).unwrap();
    assert!(
        (b.x0 + 10.0).abs() < 0.01 && (b.y1 - 10.0).abs() < 0.01,
        "{b:?}"
    );
}

#[test]
fn view_box_and_aspect_ratio() {
    let vb = ViewBox::parse("0,0 100 50").unwrap();
    assert!(ViewBox::parse("0 0 -1 5").is_none());
    let par = AspectRatio::parse("xMidYMid meet").unwrap();
    let m = par.transform(&vb, 200.0, 200.0);
    assert_eq!(m.as_coeffs(), [2.0, 0.0, 0.0, 2.0, 0.0, 50.0]);
    assert_eq!(
        AspectRatio::parse("none slice").unwrap().to_css(),
        "none slice"
    );
    assert!(AspectRatio::parse("xmidymid").is_none());
}

#[test]
fn marker_vertices_of_a_path() {
    let vertices = |d: &str| {
        PathData::parse(d)
            .marker_vertices()
            .iter()
            .map(|(p, a)| (p.x, p.y, (a.to_degrees() * 100.0).round() / 100.0))
            .collect::<Vec<_>>()
    };
    // (the ends of an open path orient along it, a corner half way between)
    assert_eq!(
        vertices("M 0 0 L 10 0 L 10 10"),
        [(0.0, 0.0, 0.0), (10.0, 0.0, 45.0), (10.0, 10.0, 90.0)]
    );
    // (a closed subpath's last vertex is its first, and both orient by its two directions)
    let closed = vertices("M 0 0 L 10 0 L 10 10 Z");
    assert_eq!(closed.len(), 4);
    assert_eq!(closed[0].2, closed[3].2);
    assert_eq!((closed[3].0, closed[3].1), (0.0, 0.0));
    // (one arc is one segment, however many curves it takes to draw)
    assert_eq!(vertices("M 0 0 A 50 50 0 1 1 100 0").len(), 2);
    // (a closepath at the start adds no vertex of its own)
    assert_eq!(vertices("M 0 0 L 10 0 L 0 0 Z").len(), 3);
}
