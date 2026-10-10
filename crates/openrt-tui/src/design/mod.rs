//! The OpenRT design system: semantic tokens, responsive layout measurement,
//! and reusable components.
//!
//! This module is the only place the TUI's visual language is defined. Views
//! and renderers compose what is here and never name a color, a magic width,
//! or a border configuration of their own.
//!
//! # Layers
//!
//! ```text
//! theme        Palette + ThemeKind + persistence          (crate::theme)
//!  └─ tokens   Surfaces / Borders / Text / Status /        (design::tokens)
//!              Interaction / Accent / Spacing /
//!              LayoutMetrics / Typography
//!     ├─ layout     Breakpoints + ShellLayout + HitTargets  (design::layout)
//!     ├─ collections ListView / TableView / TreeView        (design::collections)
//!     ├─ input      Field / Search / caret viewport          (design::input)
//!     ├─ feedback   Toast / Notification                      (design::feedback)
//!     ├─ focus      FocusTarget / FocusModel / Interaction     (design::focus)
//!     ├─ widgets  Panel, Badge, Button, ...                 (design::widgets)
//!     ├─ shell    Header / StatusBar / Rail / AppShell      (design::shell)
//!     └─ states   Loading / Empty / Error / Info            (design::states)
//!  └─ views     Chat, Files, Security, ...                   (agent::render)
//! ```
//!
//! # Invariants
//!
//! * A component never reads a [`Palette`] directly — only [`Tokens`].
//! * No module outside `theme` contains an `Rgb` literal.
//! * Every allocation of width or height comes from [`LayoutMetrics`] or a
//!   [`Breakpoint`] decision.
//! * Nothing may panic or index on a viewport dimension; layout arithmetic
//!   uses [`tokens::saturating_sub`].

pub mod collections;
pub mod contrast;
pub mod feedback;
pub mod focus;
pub mod input;
pub mod layout;
pub mod modals;
pub mod semantics;
pub mod shell;
pub mod states;
pub mod text;
pub mod tokens;
pub mod widgets;

#[cfg(test)]
pub(crate) use collections::{list_rows_materialised, reset_list_rows_materialised};
pub use collections::{
    Align, Column, FilteredWindow, ListItem, ListView, RowTone, TableRows, TableView, TreeNode,
    TreeView,
};
pub use contrast::ratio as contrast_ratio;
pub use feedback::{draw_notification, draw_toasts, Toast, ToastQueue, ToastTone};
pub use focus::{FocusModel, FocusStop, FocusTarget, Interaction, Move, OverlayKeys, OverlayScope};
pub use input::{draw_field, draw_search};
pub use layout::{Breakpoint, HitTarget, HitTargets, PanelId, ShellLayout};
pub use modals::{ConfirmDialog, Grant, PermissionDialog, PermissionRequest, Risk};
pub use semantics::Semantics;
pub use shell::{AppShell, RailItem, RailRow};
pub use states::{State, StateKind};
pub use text::{
    caret_column, chars_in_width, display_width, pad_right, truncate_width, Truncation,
};
pub use tokens::Tokens;
pub use widgets::{
    divider, ellipsize, key_hint, key_hints, metric, panel, progress_bar, render_panel,
    sanitize_field, spinner, tab_bar, Badge, BadgeTone, ButtonGroup, ButtonSpec, PanelTone,
};
