use owui::layout::{ClipBounds, Point};

#[test]
fn clip_bounds_membership_is_half_open_on_both_axes() {
    let clip = ClipBounds {
        min: Point::new(2.0, 3.0),
        max: Point::new(8.0, 9.0),
    };

    assert!(clip.contains(Point::new(2.0, 3.0)));
    assert!(clip.contains(Point::new(7.999, 8.999)));
    assert!(!clip.contains(Point::new(8.0, 5.0)));
    assert!(!clip.contains(Point::new(5.0, 9.0)));

    let touching = clip.intersect(ClipBounds {
        min: Point::new(8.0, 4.0),
        max: Point::new(10.0, 6.0),
    });
    assert!(touching.is_empty());
    assert!(!touching.contains(Point::new(8.0, 5.0)));
}
