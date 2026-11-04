//! The bar's cached content: the two halves as drawn, their colours and the
//! hit box of every widget.

use crate::actions::Target;
use crate::draw::{self, HitBox};
use crate::settings;
use crate::WsEntry;

/// Clickable regions currently on screen, republished every time the text
/// changes.
#[derive(Default)]
pub struct Clickable {
    /// Hit box and what it activates, in render order.
    pub regions: Vec<(HitBox, Target)>,
}

impl Clickable {
    /// What sits at `x`, if anything.
    pub fn target_at(&self, x: i32) -> Option<&Target> {
        self.regions
            .iter()
            .find(|(b, _)| b.contains(x))
            .map(|(_, target)| target)
    }
}

/// Cached zone texts. Each source refreshes on its own schedule — a timer, or a
/// push event — and the bar only redraws when a string actually changed.
#[derive(Default)]
pub struct Zones {
    pub left: String,
    pub left_colors: Vec<u32>,
    pub right: String,
    pub right_colors: Vec<u32>,
    pub clickable: Clickable,
    /// Per-module state: sampling baselines, live connections, cached helpers.
    pub modules: crate::modules::layout::State,
}

impl Zones {
    /// Recomputes both halves and republishes their hit boxes. Returns true
    /// when either changed and a repaint is needed.
    pub fn refresh(&mut self, font: &fontdue::Font, width: u32, workspaces: &[WsEntry]) -> bool {
        let bar = crate::modules::layout::bar(&mut self.modules, workspaces);

        // The elastic segment takes what the right half leaves: shrink it
        // before measuring, so positions and hit boxes see the final text.
        let room = width as i32
            - 2 * settings::PAD
            - draw::zone_width(font, &bar.right.text)
            - settings::WIDGET_GAP;
        let left_text = draw::fit_elastic(font, room, &bar.left.text, bar.left.elastic);

        // A dropped elastic segment takes its colour and target with it.
        let sep = crate::modules::layout::SEP;
        let dropped = bar
            .left
            .elastic
            .filter(|_| left_text.split(sep).count() < bar.left.text.split(sep).count());
        let (mut left_targets, mut left_colors) = (bar.left.targets, bar.left.colors);
        if let Some(i) = dropped {
            if i < left_targets.len() {
                left_targets.remove(i);
                left_colors.remove(i);
            }
        }

        let changed = differs(
            (&left_text, &left_colors),
            (&bar.right.text, &bar.right.colors),
            (&self.left, &self.left_colors),
            (&self.right, &self.right_colors),
        );
        self.left = left_text;
        self.left_colors = left_colors;
        self.right = bar.right.text;
        self.right_colors = bar.right.colors;

        self.publish_regions(font, width, left_targets, bar.right.targets);
        changed
    }

    /// Maps both halves onto hit boxes, one per widget: the left from the left
    /// padding, the right from its anchor against the right edge.
    fn publish_regions(
        &mut self,
        font: &fontdue::Font,
        width: u32,
        left_targets: Vec<Target>,
        right_targets: Vec<Target>,
    ) {
        let mut regions: Vec<(HitBox, Target)> =
            draw::segment_hitboxes(font, settings::PAD, &self.left)
                .into_iter()
                .zip(left_targets)
                .collect();
        let anchor = draw::right_anchor(font, width, &self.right);
        regions.extend(
            draw::segment_hitboxes(font, anchor, &self.right)
                .into_iter()
                .zip(right_targets),
        );
        self.clickable.regions = regions;
    }

    /// Draws the cached content into a fresh `width * height` ARGB8888 buffer.
    pub fn render(&self, font: &fontdue::Font, width: u32, height: u32) -> Vec<u8> {
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        draw::draw_bar(
            &mut pixels,
            width,
            height,
            font,
            &self.left,
            &self.left_colors,
            &self.right,
            &self.right_colors,
        );
        pixels
    }
}

/// Whether the bar's appearance changed and has to be repainted.
///
/// Colours count, not just text. Several widgets say what they mean *only*
/// through colour — an urgent workspace keeps its number and turns red, the CPU
/// keeps its percentage and shifts hue with the power level — so comparing text
/// alone leaves the bar showing a stale colour until something else moves.
fn differs(
    left: (&str, &[u32]),
    right: (&str, &[u32]),
    was_left: (&str, &[u32]),
    was_right: (&str, &[u32]),
) -> bool {
    left.0 != was_left.0
        || right.0 != was_right.0
        || left.1 != was_left.1
        || right.1 != was_right.1
}

#[cfg(test)]
mod repaint_tests {
    use super::differs;

    /// The case that went wrong: a workspace turning red keeps its label, so a
    /// text-only comparison saw nothing and the mark stayed invisible until
    /// some other widget happened to change.
    #[test]
    fn a_colour_change_alone_is_a_change() {
        const GREY: u32 = 0xE6E6E6;
        const RED: u32 = 0xFF6B6B;
        assert!(
            differs(
                ("1\u{1f}chat", &[GREY, RED]),
                ("12:00", &[GREY]),
                ("1\u{1f}chat", &[GREY, GREY]),
                ("12:00", &[GREY]),
            ),
            "the chat workspace turned red: repaint"
        );
    }

    /// Identical text and colours must not repaint: the bar wakes on every
    /// event source, and repainting each time would undo the idle-CPU work.
    #[test]
    fn an_identical_bar_is_not_a_change() {
        const C: u32 = 0xE6E6E6;
        assert!(!differs(
            ("1\u{1f}chat", &[C, C]),
            ("12:00", &[C]),
            ("1\u{1f}chat", &[C, C]),
            ("12:00", &[C]),
        ));
    }

    /// Text changes still count, on either half, and so does a colour change
    /// confined to the right half — the CPU widget shifts hue by power level.
    #[test]
    fn either_half_can_trigger_a_repaint() {
        const C: u32 = 0xE6E6E6;
        const HOT: u32 = 0xE65C5C;
        assert!(differs(("a", &[C]), ("x", &[C]), ("b", &[C]), ("x", &[C])));
        assert!(differs(("a", &[C]), ("x", &[C]), ("a", &[C]), ("y", &[C])));
        assert!(differs(("a", &[C]), ("x", &[C]), ("a", &[C]), ("x", &[HOT])));
        // A segment appearing or disappearing changes the colour list length.
        assert!(differs(("a", &[C, C]), ("x", &[C]), ("a", &[C]), ("x", &[C])));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::Widget;
    use crate::modules::workspaces::{entry, label};

    /// Each workspace is a segment of its own, so a click resolves to the one
    /// drawn at that x — no special case, unlike when they shared a segment.
    #[test]
    fn clicks_route_to_the_workspace_under_the_pointer() {
        let font = crate::draw::load_font().unwrap();
        let list = [entry(-1337, "chat"), entry(-98, "special:resize"), entry(31, "31"), entry(33, "33")];
        let sep = crate::modules::layout::SEP;
        // One segment per workspace, as the layout builds them.
        let text = list.iter().map(label).collect::<Vec<_>>().join(sep);
        assert_eq!(text.split(sep).collect::<Vec<_>>(), ["chat", "resize", "A", "C"]);

        let boxes = crate::draw::segment_hitboxes(&font, 800, &text);
        let c = Clickable {
            regions: boxes
                .iter()
                .zip(list.iter())
                .map(|(b, w)| (*b, Target::Workspace { id: w.id, name: w.name.clone() }))
                .collect(),
        };

        for (b, expected) in boxes.iter().zip([-1337, -98, 31, 33]) {
            let mid = (b.x0 + b.x1) / 2;
            match c.target_at(mid) {
                Some(Target::Workspace { id, .. }) => assert_eq!(*id, expected, "at x={mid}"),
                other => panic!("x={mid} resolved to {other:?}"),
            }
        }

        assert!(c.target_at(0).is_none(), "left of PAD must not hit");
        assert!(c.target_at(boxes[3].x1 + 50).is_none(), "past text");

        // Special workspaces keep their prefixed name, so the compositor can
        // toggle them instead of focusing them.
        let mid = (boxes[1].x0 + boxes[1].x1) / 2;
        assert_eq!(
            c.target_at(mid),
            Some(&Target::Workspace { id: -98, name: "special:resize".into() })
        );
    }

    /// Right-zone widgets get their own boxes, and a multi-word widget is one
    /// box rather than one per word.
    #[test]
    fn clicks_route_to_right_zone_widgets() {
        let font = crate::draw::load_font().unwrap();
        // Two segments, the first of which renders three words.
        let sep = crate::modules::layout::SEP;
        let text = format!("\u{f028} 80% \u{f130} 22%{sep}\u{f017} 12:34");
        let boxes = crate::draw::segment_hitboxes(&font, 1000, &text);
        assert_eq!(boxes.len(), 2, "one box per segment: {boxes:?}");

        let c = Clickable {
            regions: vec![
                (boxes[0], Target::Widget(Widget::Volume)),
                (boxes[1], Target::Widget(Widget::Timer)),
            ],
        };

        // Anywhere inside the volume segment, including between its words.
        for x in [boxes[0].x0, (boxes[0].x0 + boxes[0].x1) / 2, boxes[0].x1 - 1] {
            assert_eq!(c.target_at(x), Some(&Target::Widget(Widget::Volume)), "at x={x}");
        }
        assert_eq!(
            c.target_at((boxes[1].x0 + boxes[1].x1) / 2),
            Some(&Target::Widget(Widget::Timer))
        );
        // The separator between segments belongs to neither.
        assert!(c.target_at(boxes[0].x1 + 1).is_none(), "separator must not hit");
    }

    /// Widgets spread across the bar are all reachable, and the gaps between
    /// them hit nothing.
    #[test]
    fn widgets_across_the_bar_are_clickable() {
        let c = Clickable {
            regions: vec![
                (HitBox { x0: 0, x1: 50 }, Target::Workspace { id: 1, name: "1".into() }),
                (HitBox { x0: 900, x1: 950 }, Target::Widget(Widget::Lid)),
            ],
        };

        assert!(matches!(c.target_at(25), Some(Target::Workspace { .. })));
        assert_eq!(c.target_at(925), Some(&Target::Widget(Widget::Lid)));
        assert!(c.target_at(500).is_none(), "the gap between widgets");
    }

    /// A refresh publishes one hit box per widget and a frame of the right
    /// size, opaque everywhere.
    #[test]
    fn a_refresh_publishes_regions_and_renders_opaque() {
        let font = crate::draw::load_font().unwrap();
        let height = settings::bar_height(&font);
        let mut zones = Zones::default();
        let mut ws = entry(1, "1");
        ws.active = true;
        assert!(zones.refresh(&font, 2560, &[ws.clone(), entry(2, "2")]), "first refresh draws");
        assert!(matches!(
            zones.clickable.regions.first(),
            Some((_, Target::Workspace { id: 1, .. }))
        ));
        let pixels = zones.render(&font, 2560, height);
        assert_eq!(pixels.len(), 2560 * height as usize * 4);
        assert!(pixels.as_chunks::<4>().0.iter().all(|px| px[3] == 0xFF), "every pixel opaque");
    }
}
