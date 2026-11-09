//! Terminal emulation: cells, colors, styles and the VT parser state machine.

pub mod cell;
pub mod screen;
pub mod style;

pub use cell::{char_width, Cell};
pub use screen::{Buffer, Charset, Cursor, Event, MouseMode, Row, Screen, Terminal};
pub use style::{attrs, Color, Style};
