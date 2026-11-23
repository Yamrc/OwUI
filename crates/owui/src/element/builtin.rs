mod button;
mod flexbox;
mod text;
mod text_input;

pub use button::Button;
pub use flexbox::Column;
pub use text::Text;
pub use text_input::TextInput;

use super::{ElementError, Height, Width};
use crate::layout::{Constraints, LayoutCx, Size};

fn constrain(
    cx: &LayoutCx<'_>,
    constraints: Constraints,
    natural: Size,
) -> Result<Size, ElementError> {
    let width = cx.get(Width);
    let height = cx.get(Height);
    if !constraints.is_valid()
        || width.is_nan()
        || height.is_nan()
        || width < 0.0
        || height < 0.0
        || !natural.is_valid()
    {
        return Err(ElementError::InvalidGeometry);
    }

    Ok(constraints.constrain(Size::new(
        if width.is_finite() {
            width
        } else {
            natural.width
        },
        if height.is_finite() {
            height
        } else {
            natural.height
        },
    )))
}
