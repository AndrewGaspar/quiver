// todo("windows"): remove
#![cfg_attr(windows, allow(dead_code))]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AtlasTextureId, AtlasTile, Background, Bounds, ContentMask, Corners, Edges, Hsla, Path,
    PathTiles, Pixels, Point, Radians, ScaledPixels, Size, TimedTransformation,
    bounds_tree::BoundsTree, clamp_eye_shift,
};
use smallvec::SmallVec;
use std::{
    collections::VecDeque, fmt::Debug, iter::Peekable, ops::Range, rc::Rc, slice, time::Duration,
};

#[expect(missing_docs)]
pub type DrawOrder = u32;

/// A boolean stored as a `u32` so that GPU-facing structs contain no
/// compiler-inserted padding bytes, which would be undefined behavior to
/// reinterpret as `&[u8]` when writing instance buffers. Guaranteed to be
/// `0` or `1` by construction; shaders read it as a `u32`/`uint`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct PaddedBool32(u32);

impl From<bool> for PaddedBool32 {
    fn from(value: bool) -> Self {
        PaddedBool32(value as u32)
    }
}

impl PaddedBool32 {
    /// The wrapped boolean.
    pub fn as_bool(self) -> bool {
        self.0 != 0
    }
}

struct Layer {
    primitives: SmallVec<[Range<usize>; 1]>,
    bounds: Bounds<ScaledPixels>,
    max_kind: PrimitiveKind,
    max_shared_kind: PrimitiveKind,
}

#[derive(Default)]
#[expect(missing_docs)]
pub struct Scene {
    pub(crate) paint_operations: Vec<PaintOperation>,
    primitive_bounds: BoundsTree<ScaledPixels>,
    layer_stack: Vec<usize>,
    layers: VecDeque<Layer>,
    pub shadows: Vec<Shadow>,
    pub quads: Vec<Quad>,
    pub paths: Vec<PaintedPath>,
    pub underlines: Vec<Underline>,
    pub monochrome_sprites: Vec<MonochromeSprite>,
    pub subpixel_sprites: Vec<SubpixelSprite>,
    pub polychrome_sprites: Vec<PolychromeSprite>,
    pub surfaces: Vec<PaintSurface>,
    animations: Vec<SpriteAnimation>,
    /// Quiver: each paint operation's stereo scope (an index into `stereo_scopes`),
    /// index-aligned with `paint_operations`. Unread unless the window packs a stereo frame.
    paint_scopes: Vec<u32>,
    /// Quiver: each primitive's stereo scope, index-aligned with its kind's vector.
    primitive_scopes: PrimitiveScopes,
    /// Quiver: this frame's depth scopes ([`Scene::push_stereo_scope`]); 0 is the plane.
    stereo_scopes: Vec<StereoScope>,
    /// Quiver: the scope primitives inserted now belong to.
    current_scope: u32,
    /// Quiver: how far (device px) primitives may move sideways when packed. Draw orders are
    /// worked out as if each primitive were that much wider on both sides, so two that only
    /// overlap once moved still draw in paint order.
    order_slack: f32,
    /// Quiver: in a packed scene, each source scope's shifts ([`Scene::packed_scope_shifts`]).
    packed_shifts: Vec<[f32; 2]>,
}

/// Quiver: a run of paint operations at one stereo depth: an element and what it paints,
/// up to its children that have depths of their own ([`Scene::push_stereo_scope`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoScope {
    /// HypXRland's unitless rung, 0 = the window's plane.
    pub depth: f32,
    /// The element's bounds (device pixels), for keeping its shift inside the eye's edges;
    /// `None` isn't clamped.
    pub bounds: Option<Bounds<ScaledPixels>>,
}

const ROOT_SCOPE: StereoScope = StereoScope {
    depth: 0.0,
    bounds: None,
};

/// Quiver: the stereo scope of every primitive, by kind.
#[derive(Default)]
struct PrimitiveScopes {
    shadows: Vec<u32>,
    quads: Vec<u32>,
    paths: Vec<u32>,
    underlines: Vec<u32>,
    monochrome_sprites: Vec<u32>,
    subpixel_sprites: Vec<u32>,
    polychrome_sprites: Vec<u32>,
    surfaces: Vec<u32>,
}

impl PrimitiveScopes {
    fn clear(&mut self) {
        self.shadows.clear();
        self.quads.clear();
        self.paths.clear();
        self.underlines.clear();
        self.monochrome_sprites.clear();
        self.subpixel_sprites.clear();
        self.polychrome_sprites.clear();
        self.surfaces.clear();
    }
}

#[expect(missing_docs)]
impl Scene {
    pub fn clear(&mut self) {
        self.paint_operations.clear();
        self.primitive_bounds.clear();
        self.layer_stack.clear();
        self.layers.clear();
        self.paths.clear();
        self.shadows.clear();
        self.quads.clear();
        self.underlines.clear();
        self.monochrome_sprites.clear();
        self.subpixel_sprites.clear();
        self.polychrome_sprites.clear();
        self.surfaces.clear();
        self.animations.clear();
        self.paint_scopes.clear();
        self.primitive_scopes.clear();
        self.stereo_scopes.clear();
        self.current_scope = 0;
    }

    fn stereo_scope(&self, index: u32) -> StereoScope {
        self.stereo_scopes
            .get(index as usize)
            .copied()
            .unwrap_or(ROOT_SCOPE)
    }

    /// Quiver: the primitives inserted from now on are at a stereo `depth` (HypXRland's
    /// unitless rung, 0 = the window's plane), kept inside the eye's edges as `bounds` (device
    /// pixels) is. Returns the previous scope, for [`Scene::pop_stereo_scope`]. Only
    /// [`Scene::pack_side_by_side`] reads it.
    pub fn push_stereo_scope(
        &mut self,
        depth: f32,
        bounds: Option<Bounds<ScaledPixels>>,
    ) -> u32 {
        if self.stereo_scopes.is_empty() {
            self.stereo_scopes.push(ROOT_SCOPE);
        }
        let index = self.stereo_scopes.len() as u32;
        self.stereo_scopes.push(StereoScope { depth, bounds });
        std::mem::replace(&mut self.current_scope, index)
    }

    /// Quiver: back to the scope [`Scene::push_stereo_scope`] returned.
    pub fn pop_stereo_scope(&mut self, previous: u32) {
        self.current_scope = previous;
    }

    /// Quiver: primitives from now on are at `depth`, unclamped; returns the previous depth.
    pub fn set_stereo_depth(&mut self, depth: f32) -> f32 {
        let previous = self.stereo_depth();
        self.push_stereo_scope(depth, None);
        previous
    }

    /// Quiver: the stereo depth primitives are being tagged with.
    pub fn stereo_depth(&self) -> f32 {
        self.stereo_scope(self.current_scope).depth
    }

    /// Quiver: the number of depth scopes so far this frame.
    pub fn stereo_scope_count(&self) -> usize {
        self.stereo_scopes.len()
    }

    /// Quiver: this frame's depth scopes, by index (0 is the window's plane).
    pub fn stereo_scopes(&self) -> &[StereoScope] {
        &self.stereo_scopes
    }

    /// Quiver: see `order_slack`. Zero (the default) leaves draw orders as upstream has them.
    pub fn set_stereo_order_slack(&mut self, device_px: f32) {
        self.order_slack = device_px.max(0.0);
    }

    /// Inserts a sprite that [`Self::advance_animations`] keeps moving. The
    /// sprite's color is taken as its color before the animation's alpha.
    pub(crate) fn insert_animated_sprite(
        &mut self,
        mut sprite: MonochromeSprite,
        mut animation: SpriteAnimation,
    ) {
        sprite.animation = SpriteAnimationId(self.animations.len() as u32);
        animation.base_color = sprite.color;
        if self.try_insert_primitive(sprite.into()) {
            self.animations.push(animation);
        }
    }

    /// How long the window may wait between presents of this scene while only
    /// its animations need a frame: `None` when nothing in it animates, and
    /// zero when an animation wants every frame.
    pub(crate) fn animation_frame_interval(&self) -> Option<Duration> {
        self.animations
            .iter()
            .map(|animation| {
                animation
                    .transformation
                    .min_frame_interval()
                    .unwrap_or_default()
            })
            .min()
    }

    /// Moves every animated sprite to its pose at `elapsed` time on the app's
    /// synced animation clock, without the scene being rebuilt.
    pub(crate) fn advance_animations(&mut self, elapsed: Duration) {
        for animation in &self.animations {
            let Some(sprite) = self.monochrome_sprites.get_mut(animation.sprite_index) else {
                continue;
            };
            let pose = animation.transformation.at(elapsed);
            sprite.transformation = pose.into_matrix(animation.center, animation.scale_factor);
            sprite.color = animation.base_color.opacity(pose.alpha_factor());
        }
    }

    pub fn len(&self) -> usize {
        self.paint_operations.len()
    }

    pub fn push_layer(&mut self) {
        self.layer_stack.push(self.layers.len());
        self.layers.push_back(Layer {
            primitives: SmallVec::new(),
            bounds: Bounds::default(),
            max_kind: PrimitiveKind::Shadow,
            max_shared_kind: PrimitiveKind::Surface,
        });
        self.paint_operations.push(PaintOperation::StartLayer);
        self.paint_scopes.push(self.current_scope);
    }

    pub fn pop_layer(&mut self) {
        self.layer_stack.pop();
        if self.layer_stack.is_empty() {
            while let Some(layer) = self.layers.pop_front() {
                self.insert_layer(layer);
            }
        }
        self.paint_operations.push(PaintOperation::EndLayer);
        self.paint_scopes.push(self.current_scope);
    }

    fn insert_layer(&mut self, layer: Layer) {
        if layer.primitives.is_empty() {
            return;
        }
        let order =
            self.primitive_bounds
                .insert(layer.bounds, layer.max_kind, layer.max_shared_kind);
        for index in layer.primitives.into_iter().flatten() {
            if let PaintOperation::Primitive(primitive) = &self.paint_operations[index] {
                let primitive = primitive.clone();
                let scope = self.paint_scopes.get(index).copied().unwrap_or(0);
                self.push_primitive(primitive, order, scope);
            }
        }
    }

    pub fn insert_primitive(&mut self, primitive: impl Into<Primitive>) {
        self.try_insert_primitive(primitive.into());
    }

    /// Returns whether the primitive was kept, rather than clipped away.
    fn try_insert_primitive(&mut self, primitive: Primitive) -> bool {
        let clipped_bounds = primitive.clipped_bounds();
        if clipped_bounds.is_empty() {
            return false;
        }

        let kind = primitive.kind();
        // Quiver: while packing, as wide as the primitive may move (see `order_slack`).
        let clipped_bounds = if self.order_slack > 0.0 {
            Bounds {
                origin: Point {
                    x: ScaledPixels(clipped_bounds.origin.x.0 - self.order_slack),
                    y: clipped_bounds.origin.y,
                },
                size: Size {
                    width: ScaledPixels(clipped_bounds.size.width.0 + 2.0 * self.order_slack),
                    height: clipped_bounds.size.height,
                },
            }
        } else {
            clipped_bounds
        };
        if let Some(&layer) = self.layer_stack.last()
            && let Some(layer) = self.layers.get_mut(layer)
        {
            layer.bounds = layer.bounds.join(&clipped_bounds);
            layer.max_kind = layer.max_kind.max(kind);
            layer.max_shared_kind = layer.max_shared_kind.min(kind.max_shared_kind());
            let index = self.paint_operations.len();
            match layer.primitives.last_mut() {
                Some(range) if range.end == index => range.end += 1,
                _ => layer.primitives.push(index..index + 1),
            }
        } else {
            let order = self
                .primitive_bounds
                .insert(clipped_bounds, kind, kind.max_shared_kind());
            self.push_primitive(primitive.clone(), order, self.current_scope);
        }
        self.paint_operations
            .push(PaintOperation::Primitive(primitive));
        self.paint_scopes.push(self.current_scope);
        true
    }

    fn push_primitive(&mut self, primitive: Primitive, order: DrawOrder, scope: u32) {
        let scopes = &mut self.primitive_scopes;
        match primitive {
            Primitive::Shadow(shadow) => {
                self.shadows.push(Shadow { order, ..shadow });
                scopes.shadows.push(scope);
            }
            Primitive::Quad(quad) => {
                self.quads.push(Quad { order, ..quad });
                scopes.quads.push(scope);
            }
            Primitive::Path(mut path) => {
                path.order = order;
                self.paths.push(path);
                scopes.paths.push(scope);
            }
            Primitive::Underline(underline) => {
                self.underlines.push(Underline { order, ..underline });
                scopes.underlines.push(scope);
            }
            Primitive::MonochromeSprite(sprite) => {
                self.monochrome_sprites
                    .push(MonochromeSprite { order, ..sprite });
                scopes.monochrome_sprites.push(scope);
            }
            Primitive::SubpixelSprite(sprite) => {
                self.subpixel_sprites
                    .push(SubpixelSprite { order, ..sprite });
                scopes.subpixel_sprites.push(scope);
            }
            Primitive::PolychromeSprite(sprite) => {
                self.polychrome_sprites
                    .push(PolychromeSprite { order, ..sprite });
                scopes.polychrome_sprites.push(scope);
            }
            Primitive::Surface(mut surface) => {
                surface.order = order;
                self.surfaces.push(surface);
                scopes.surfaces.push(scope);
            }
        }
    }

    pub fn replay(&mut self, range: Range<usize>, prev_scene: &Scene) {
        let outer_scope = self.current_scope;
        // Quiver: a cached view keeps the depths it was painted at: its scopes are copied
        // into this scene once each.
        let mut copied: SmallVec<[(u32, u32); 16]> = SmallVec::new();
        for (operation, &scope) in prev_scene.paint_operations[range.clone()]
            .iter()
            .zip(&prev_scene.paint_scopes[range])
        {
            self.current_scope = if scope == 0 {
                outer_scope
            } else if let Some((_, new)) = copied.iter().find(|(old, _)| *old == scope) {
                *new
            } else {
                let s = prev_scene.stereo_scope(scope);
                self.push_stereo_scope(s.depth, s.bounds);
                let new = self.current_scope;
                copied.push((scope, new));
                new
            };
            match operation {
                PaintOperation::Primitive(primitive) => {
                    // Animation ids index the scene they were inserted into.
                    if let Primitive::MonochromeSprite(sprite) = primitive
                        && let Some(animation) =
                            prev_scene.animations.get(sprite.animation.0 as usize)
                    {
                        self.insert_animated_sprite(*sprite, *animation);
                    } else {
                        self.insert_primitive(primitive.clone());
                    }
                }
                PaintOperation::StartLayer => self.push_layer(),
                PaintOperation::EndLayer => self.pop_layer(),
            }
        }
        self.current_scope = outer_scope;
    }

    /// Quiver: this (finished) scene drawn twice, side by side, into a frame two eyes wide:
    /// HypXRland's `stereo:sbs` packing (the left eye is the left half). `eye` is one eye's
    /// size in device pixels; `shift(depth)` gives each eye's horizontal offset for primitives
    /// at `depth`, in device pixels (`[left, right]`; crossed disparity, in front of the
    /// screen, is the left eye's content moving right and the right eye's moving left).
    ///
    /// The frame's edges (HypXRland's `depth_edge_slack` rule): each depth scope's shift is
    /// limited by the margin between what the scope paints and the eye's edge it moves toward
    /// ([`clamp_eye_shift`], with `edge_slack` device px of slack), so nothing slides off one
    /// eye only. A primitive spanning the eye's whole width (a scrim, a bar's background)
    /// can't move without uncovering a strip at one edge, so it stays put, and doesn't hold
    /// the rest of its scope back either. Each primitive's content mask moves with it and is
    /// clipped to its eye, so nothing crosses the seam.
    ///
    /// Both eyes' copies keep the primitive's draw order (they never overlap), so the packed
    /// scene is built straight from the sorted primitives, without working out orders again;
    /// the window widens primitives by the largest shift while ordering them
    /// ([`Scene::set_stereo_order_slack`]) so moved neighbours still draw in paint order.
    /// Animated sprites keep animating ([`Scene::advance_animations`]) in both eyes.
    pub fn pack_side_by_side(
        &self,
        eye: Size<ScaledPixels>,
        shift: &dyn Fn(f32) -> [f32; 2],
        edge_slack: f32,
    ) -> Scene {
        let scope_count = self.stereo_scopes.len().max(1);
        let scopes = &self.primitive_scopes;
        // A primitive covering the eye from edge to edge stays put.
        let spans = |b: &Bounds<ScaledPixels>| {
            b.origin.x.0 <= 0.5 && b.origin.x.0 + b.size.width.0 >= eye.width.0 - 0.5
        };
        // What each scope paints, horizontally: [left, right] in device px.
        let mut extent: Vec<[f32; 2]> = vec![[f32::MAX, f32::MIN]; scope_count];
        let mut grow = |scope: u32, b: Bounds<ScaledPixels>| {
            if b.size.width.0 <= 0.0 || b.size.height.0 <= 0.0 || spans(&b) {
                return;
            }
            if let Some(e) = extent.get_mut(scope as usize) {
                e[0] = e[0].min(b.origin.x.0);
                e[1] = e[1].max(b.origin.x.0 + b.size.width.0);
            }
        };
        macro_rules! extents {
            ($kind:ident, $clipped:expr) => {
                for (index, primitive) in self.$kind.iter().enumerate() {
                    grow(
                        scopes.$kind.get(index).copied().unwrap_or(0),
                        $clipped(primitive),
                    );
                }
            };
        }
        extents!(shadows, |p: &Shadow| p.bounds.intersect(&p.content_mask.bounds));
        extents!(quads, |p: &Quad| p.bounds.intersect(&p.content_mask.bounds));
        extents!(paths, |p: &PaintedPath| p
            .bounds
            .intersect(&p.paint.content_mask.bounds));
        extents!(underlines, |p: &Underline| p
            .bounds
            .intersect(&p.content_mask.bounds));
        extents!(monochrome_sprites, |p: &MonochromeSprite| p
            .transformation
            .transform_bounds(p.bounds)
            .intersect(&p.content_mask.bounds));
        extents!(subpixel_sprites, |p: &SubpixelSprite| p
            .transformation
            .transform_bounds(p.bounds)
            .intersect(&p.content_mask.bounds));
        extents!(polychrome_sprites, |p: &PolychromeSprite| p
            .bounds
            .intersect(&p.content_mask.bounds));
        extents!(surfaces, |p: &PaintSurface| p
            .bounds
            .intersect(&p.content_mask.bounds));

        // Each scope's two shifts, held inside the eye's edges.
        let mut memo: SmallVec<[(f32, [f32; 2]); 8]> = SmallVec::new();
        let mut shifts: Vec<[f32; 2]> = Vec::with_capacity(scope_count);
        for (index, e) in extent.iter().enumerate() {
            let scope = self.stereo_scope(index as u32);
            if scope.depth <= 0.0 || e[0] > e[1] {
                shifts.push([0.0, 0.0]);
                continue;
            }
            let s = match memo.iter().find(|(d, _)| *d == scope.depth) {
                Some((_, s)) => *s,
                None => {
                    let s = shift(scope.depth);
                    memo.push((scope.depth, s));
                    s
                }
            };
            let (left, right) = (e[0], eye.width.0 - e[1]);
            shifts.push([
                clamp_eye_shift(s[0], left, right, edge_slack),
                clamp_eye_shift(s[1], left, right, edge_slack),
            ]);
        }
        let eyes = [0.0f32, 1.0].map(|i| Bounds {
            origin: Point {
                x: ScaledPixels(eye.width.0 * i),
                y: ScaledPixels(0.0),
            },
            size: eye,
        });
        let dx = |scope: u32, e: usize, pinned: bool| {
            let own = if pinned {
                0.0
            } else {
                shifts.get(scope as usize).map_or(0.0, |s| s[e])
            };
            eyes[e].origin.x.0 + own
        };

        let mut out = Scene::default();
        macro_rules! pack {
            ($kind:ident, $clipped:expr) => {
                out.$kind.reserve(self.$kind.len() * 2);
                for (index, primitive) in self.$kind.iter().enumerate() {
                    let scope = scopes.$kind.get(index).copied().unwrap_or(0);
                    let pinned = spans(&$clipped(primitive));
                    for e in 0..2 {
                        out.$kind
                            .push(primitive.translated(dx(scope, e, pinned), &eyes[e]));
                    }
                }
            };
        }
        pack!(shadows, |p: &Shadow| p.bounds.intersect(&p.content_mask.bounds));
        pack!(quads, |p: &Quad| p.bounds.intersect(&p.content_mask.bounds));
        pack!(paths, |p: &PaintedPath| p
            .bounds
            .intersect(&p.paint.content_mask.bounds));
        pack!(underlines, |p: &Underline| p
            .bounds
            .intersect(&p.content_mask.bounds));
        pack!(subpixel_sprites, |p: &SubpixelSprite| p
            .transformation
            .transform_bounds(p.bounds)
            .intersect(&p.content_mask.bounds));
        pack!(polychrome_sprites, |p: &PolychromeSprite| p
            .bounds
            .intersect(&p.content_mask.bounds));
        pack!(surfaces, |p: &PaintSurface| p
            .bounds
            .intersect(&p.content_mask.bounds));
        out.monochrome_sprites
            .reserve(self.monochrome_sprites.len() * 2);
        for (index, sprite) in self.monochrome_sprites.iter().enumerate() {
            let scope = scopes.monochrome_sprites.get(index).copied().unwrap_or(0);
            let pinned = spans(
                &sprite
                    .transformation
                    .transform_bounds(sprite.bounds)
                    .intersect(&sprite.content_mask.bounds),
            );
            for e in 0..2 {
                let dx = dx(scope, e, pinned);
                let mut copy = sprite.translated(dx, &eyes[e]);
                if let Some(animation) = self.animations.get(sprite.animation.0 as usize) {
                    // The eye's copy animates about its own, moved, centre.
                    let mut animation = *animation;
                    animation.sprite_index = out.monochrome_sprites.len();
                    animation.center.x += Pixels(dx / animation.scale_factor.max(f32::EPSILON));
                    copy.animation = SpriteAnimationId(out.animations.len() as u32);
                    out.animations.push(animation);
                } else {
                    copy.animation = SpriteAnimationId::NONE;
                }
                out.monochrome_sprites.push(copy);
            }
        }
        out.packed_shifts = shifts;
        out
    }

    /// Quiver: in a packed scene, each depth scope's `[left, right]` shift as packed (after
    /// the edge clamp), by scope index of the scene it was packed from.
    pub fn packed_scope_shifts(&self) -> &[[f32; 2]] {
        &self.packed_shifts
    }

    /// Quiver: how many primitives the scene has, of every kind.
    pub fn primitive_count(&self) -> usize {
        self.shadows.len()
            + self.quads.len()
            + self.paths.len()
            + self.underlines.len()
            + self.monochrome_sprites.len()
            + self.subpixel_sprites.len()
            + self.polychrome_sprites.len()
            + self.surfaces.len()
    }

    /// Quiver: the stereo depth of each quad, in [`Scene::quads`]' order (tests, evidence).
    pub fn quad_stereo_depths(&self) -> Vec<f32> {
        self.primitive_scopes
            .quads
            .iter()
            .map(|&s| self.stereo_scope(s).depth)
            .collect()
    }

    pub fn finish(&mut self) {
        debug_assert!(self.layer_stack.is_empty(), "unbalanced scene layers");
        let keys = &mut Vec::new();
        // Quiver: each kind's stereo scopes move with its primitives.
        let scopes = &mut self.primitive_scopes;
        sort_in_place(&mut self.shadows, &mut scopes.shadows, keys, |shadow| {
            (shadow.order, 0)
        });
        sort_in_place(&mut self.quads, &mut scopes.quads, keys, |quad| (quad.order, 0));
        sort_by_order(&mut self.paths, &mut scopes.paths, |path| path.order);
        sort_in_place(&mut self.underlines, &mut scopes.underlines, keys, |underline| {
            (underline.order, 0)
        });
        sort_in_place(
            &mut self.monochrome_sprites,
            &mut scopes.monochrome_sprites,
            keys,
            |sprite| (sprite.order, sprite.tile.texture_id.index),
        );
        sort_in_place(
            &mut self.subpixel_sprites,
            &mut scopes.subpixel_sprites,
            keys,
            |sprite| (sprite.order, sprite.tile.texture_id.index),
        );
        sort_in_place(
            &mut self.polychrome_sprites,
            &mut scopes.polychrome_sprites,
            keys,
            |sprite| (sprite.order, sprite.tile.texture_id.index),
        );
        sort_by_order(&mut self.surfaces, &mut scopes.surfaces, |surface| {
            surface.order
        });

        // Sorting moved the sprites, so animations learn where theirs ended up.
        if !self.animations.is_empty() {
            for (index, sprite) in self.monochrome_sprites.iter().enumerate() {
                if let Some(animation) = self.animations.get_mut(sprite.animation.0 as usize) {
                    animation.sprite_index = index;
                }
            }
        }
    }

    #[cfg_attr(
        all(
            any(target_os = "linux", target_os = "freebsd"),
            not(any(feature = "x11", feature = "wayland"))
        ),
        allow(dead_code)
    )]
    pub fn batches(&self) -> impl Iterator<Item = PrimitiveBatch> + '_ {
        BatchIterator {
            shadows_start: 0,
            shadows_iter: self.shadows.iter().peekable(),
            quads_start: 0,
            quads_iter: self.quads.iter().peekable(),
            paths_start: 0,
            paths_iter: self.paths.iter().peekable(),
            underlines_start: 0,
            underlines_iter: self.underlines.iter().peekable(),
            monochrome_sprites_start: 0,
            monochrome_sprites_iter: self.monochrome_sprites.iter().peekable(),
            subpixel_sprites_start: 0,
            subpixel_sprites_iter: self.subpixel_sprites.iter().peekable(),
            polychrome_sprites_start: 0,
            polychrome_sprites_iter: self.polychrome_sprites.iter().peekable(),
            surfaces_start: 0,
            surfaces_iter: self.surfaces.iter().peekable(),
        }
    }
}

/// Sorts small `(key, index)` pairs instead of the primitives themselves, then moves each
/// primitive into place once. Primitives are large, so a comparison sort that shuffles them
/// directly spends most of its time copying. Ties keep insertion order, as `sort_by_key`
/// does, because the index is part of what is sorted.
fn sort_in_place<T: Copy>(
    primitives: &mut [T],
    // Quiver: the primitives' stereo scopes, moved with them.
    scopes: &mut [u32],
    keys: &mut Vec<(u64, u32)>,
    key: impl Fn(&T) -> (u32, u32),
) {
    debug_assert_eq!(primitives.len(), scopes.len());
    if primitives.is_sorted_by_key(&key) {
        return;
    }
    let with_scopes = scopes.len() == primitives.len();
    keys.clear();
    keys.extend(primitives.iter().enumerate().map(|(index, primitive)| {
        let (major, minor) = key(primitive);
        ((major as u64) << 32 | minor as u64, index as u32)
    }));
    keys.sort_unstable();

    // `keys[target].1` is the source index for `target`. Follow each cycle of that
    // permutation, marking visited slots by pointing them at themselves.
    for start in 0..primitives.len() {
        if keys[start].1 as usize == start {
            continue;
        }
        let displaced = primitives[start];
        let displaced_scope = if with_scopes { scopes[start] } else { 0 };
        let mut target = start;
        loop {
            let source = keys[target].1 as usize;
            keys[target].1 = target as u32;
            if source == start {
                primitives[target] = displaced;
                if with_scopes {
                    scopes[target] = displaced_scope;
                }
                break;
            }
            primitives[target] = primitives[source];
            if with_scopes {
                scopes[target] = scopes[source];
            }
            target = source;
        }
    }
}

/// Quiver: a stable sort of non-`Copy` primitives (paths, surfaces) by draw order, their
/// stereo scopes moved with them.
fn sort_by_order<T>(primitives: &mut Vec<T>, scopes: &mut Vec<u32>, order: impl Fn(&T) -> u32) {
    if primitives.is_sorted_by_key(&order) {
        return;
    }
    let mut indices: Vec<usize> = (0..primitives.len()).collect();
    indices.sort_by_key(|&i| order(&primitives[i]));
    let mut taken: Vec<Option<T>> = primitives.drain(..).map(Some).collect();
    let with_scopes = scopes.len() == taken.len();
    let old_scopes = std::mem::take(scopes);
    for i in indices {
        if let Some(p) = taken[i].take() {
            primitives.push(p);
        }
        if with_scopes {
            scopes.push(old_scopes[i]);
        }
    }
}

/// The depth step between consecutive quads in a frame with `quad_count` quads.
pub fn quad_depth_scale(quad_count: usize) -> f32 {
    let denominator = (quad_count.min((1 << 24) - 1) + 1).next_power_of_two();
    1.0 / denominator as f32
}

/// Maps a quad's index in [`Scene::quads`] to depth, with greater values closer.
pub fn quad_depth(quad_id: u32, scale: f32) -> f32 {
    ((quad_id + 1) as f32 * scale).min(1.0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Default)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
pub(crate) enum PrimitiveKind {
    Shadow,
    #[default]
    Quad,
    Path,
    Underline,
    MonochromeSprite,
    SubpixelSprite,
    PolychromeSprite,
    Surface,
}

impl PrimitiveKind {
    /// We guarantee that overlapping primitives are drawn in paint order.
    /// But, to improve GPU draw batching, the renderer sorts primitives that
    /// share a draw order by kind, then by atlas texture, and only then in
    /// paint order. So, a primitive can only share the draw order of the
    /// topmost primitive it overlaps if that primitive's kind is at most the
    /// one returned here. Otherwise, it needs a new draw order.
    fn max_shared_kind(self) -> Self {
        match self {
            Self::MonochromeSprite => Self::Underline,
            Self::SubpixelSprite => Self::MonochromeSprite,
            Self::PolychromeSprite => Self::SubpixelSprite,
            kind => kind,
        }
    }
}

pub(crate) enum PaintOperation {
    Primitive(Primitive),
    StartLayer,
    EndLayer,
}

#[derive(Clone)]
#[expect(missing_docs)]
pub enum Primitive {
    Shadow(Shadow),
    Quad(Quad),
    Path(PaintedPath),
    Underline(Underline),
    MonochromeSprite(MonochromeSprite),
    SubpixelSprite(SubpixelSprite),
    PolychromeSprite(PolychromeSprite),
    Surface(PaintSurface),
}

#[expect(missing_docs)]
impl Primitive {
    pub(crate) fn kind(&self) -> PrimitiveKind {
        match self {
            Primitive::Shadow(_) => PrimitiveKind::Shadow,
            Primitive::Quad(_) => PrimitiveKind::Quad,
            Primitive::Path(_) => PrimitiveKind::Path,
            Primitive::Underline(_) => PrimitiveKind::Underline,
            Primitive::MonochromeSprite(_) => PrimitiveKind::MonochromeSprite,
            Primitive::SubpixelSprite(_) => PrimitiveKind::SubpixelSprite,
            Primitive::PolychromeSprite(_) => PrimitiveKind::PolychromeSprite,
            Primitive::Surface(_) => PrimitiveKind::Surface,
        }
    }

    pub fn bounds(&self) -> &Bounds<ScaledPixels> {
        match self {
            Primitive::Shadow(shadow) => &shadow.bounds,
            Primitive::Quad(quad) => &quad.bounds,
            Primitive::Path(path) => &path.bounds,
            Primitive::Underline(underline) => &underline.bounds,
            Primitive::MonochromeSprite(sprite) => &sprite.bounds,
            Primitive::SubpixelSprite(sprite) => &sprite.bounds,
            Primitive::PolychromeSprite(sprite) => &sprite.bounds,
            Primitive::Surface(surface) => &surface.bounds,
        }
    }

    fn transformation(&self) -> TransformationMatrix {
        match self {
            Primitive::MonochromeSprite(sprite) => sprite.transformation,
            Primitive::SubpixelSprite(sprite) => sprite.transformation,
            _ => TransformationMatrix::unit(),
        }
    }

    fn clipped_bounds(&self) -> Bounds<ScaledPixels> {
        self.transformation()
            .transform_bounds(*self.bounds())
            .intersect(&self.content_mask().bounds)
    }

    pub fn content_mask(&self) -> &ContentMask<ScaledPixels> {
        match self {
            Primitive::Shadow(shadow) => &shadow.content_mask,
            Primitive::Quad(quad) => &quad.content_mask,
            Primitive::Path(path) => &path.paint.content_mask,
            Primitive::Underline(underline) => &underline.content_mask,
            Primitive::MonochromeSprite(sprite) => &sprite.content_mask,
            Primitive::SubpixelSprite(sprite) => &sprite.content_mask,
            Primitive::PolychromeSprite(sprite) => &sprite.content_mask,
            Primitive::Surface(surface) => &surface.content_mask,
        }
    }
}

#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
struct BatchIterator<'a> {
    shadows_start: usize,
    shadows_iter: Peekable<slice::Iter<'a, Shadow>>,
    quads_start: usize,
    quads_iter: Peekable<slice::Iter<'a, Quad>>,
    paths_start: usize,
    paths_iter: Peekable<slice::Iter<'a, PaintedPath>>,
    underlines_start: usize,
    underlines_iter: Peekable<slice::Iter<'a, Underline>>,
    monochrome_sprites_start: usize,
    monochrome_sprites_iter: Peekable<slice::Iter<'a, MonochromeSprite>>,
    subpixel_sprites_start: usize,
    subpixel_sprites_iter: Peekable<slice::Iter<'a, SubpixelSprite>>,
    polychrome_sprites_start: usize,
    polychrome_sprites_iter: Peekable<slice::Iter<'a, PolychromeSprite>>,
    surfaces_start: usize,
    surfaces_iter: Peekable<slice::Iter<'a, PaintSurface>>,
}

impl<'a> Iterator for BatchIterator<'a> {
    type Item = PrimitiveBatch;

    fn next(&mut self) -> Option<Self::Item> {
        let mut orders_and_kinds = [
            (
                self.shadows_iter.peek().map(|s| s.order),
                PrimitiveKind::Shadow,
            ),
            (self.quads_iter.peek().map(|q| q.order), PrimitiveKind::Quad),
            (
                self.paths_iter.peek().map(|path| path.order),
                PrimitiveKind::Path,
            ),
            (
                self.underlines_iter.peek().map(|u| u.order),
                PrimitiveKind::Underline,
            ),
            (
                self.monochrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::MonochromeSprite,
            ),
            (
                self.subpixel_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::SubpixelSprite,
            ),
            (
                self.polychrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::PolychromeSprite,
            ),
            (
                self.surfaces_iter.peek().map(|s| s.order),
                PrimitiveKind::Surface,
            ),
        ];
        orders_and_kinds.sort_by_key(|(order, kind)| (order.unwrap_or(u32::MAX), *kind));

        let first = orders_and_kinds[0];
        let second = orders_and_kinds[1];
        let (batch_kind, max_order_and_kind) = if first.0.is_some() {
            (first.1, (second.0.unwrap_or(u32::MAX), second.1))
        } else {
            return None;
        };

        match batch_kind {
            PrimitiveKind::Shadow => {
                let shadows_start = self.shadows_start;
                let mut shadows_end = shadows_start + 1;
                self.shadows_iter.next();
                while self
                    .shadows_iter
                    .next_if(|shadow| (shadow.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    shadows_end += 1;
                }
                self.shadows_start = shadows_end;
                Some(PrimitiveBatch::Shadows(shadows_start..shadows_end))
            }
            PrimitiveKind::Quad => {
                let quads_start = self.quads_start;
                let mut quads_end = quads_start + 1;
                self.quads_iter.next();
                while self
                    .quads_iter
                    .next_if(|quad| (quad.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    quads_end += 1;
                }
                self.quads_start = quads_end;
                Some(PrimitiveBatch::Quads(quads_start..quads_end))
            }
            PrimitiveKind::Path => {
                let paths_start = self.paths_start;
                let mut paths_end = paths_start + 1;
                self.paths_iter.next();
                while self
                    .paths_iter
                    .next_if(|path| (path.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    paths_end += 1;
                }
                self.paths_start = paths_end;
                Some(PrimitiveBatch::Paths(paths_start..paths_end))
            }
            PrimitiveKind::Underline => {
                let underlines_start = self.underlines_start;
                let mut underlines_end = underlines_start + 1;
                self.underlines_iter.next();
                while self
                    .underlines_iter
                    .next_if(|underline| (underline.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    underlines_end += 1;
                }
                self.underlines_start = underlines_end;
                Some(PrimitiveBatch::Underlines(underlines_start..underlines_end))
            }
            PrimitiveKind::MonochromeSprite => {
                let texture_id = self.monochrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.monochrome_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.monochrome_sprites_iter.next();
                while self
                    .monochrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.monochrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::SubpixelSprite => {
                let texture_id = self.subpixel_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.subpixel_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.subpixel_sprites_iter.next();
                while self
                    .subpixel_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.subpixel_sprites_start = sprites_end;
                Some(PrimitiveBatch::SubpixelSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::PolychromeSprite => {
                let texture_id = self.polychrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.polychrome_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.polychrome_sprites_iter.next();
                while self
                    .polychrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.polychrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::Surface => {
                let surfaces_start = self.surfaces_start;
                let mut surfaces_end = surfaces_start + 1;
                self.surfaces_iter.next();
                while self
                    .surfaces_iter
                    .next_if(|surface| (surface.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    surfaces_end += 1;
                }
                self.surfaces_start = surfaces_end;
                Some(PrimitiveBatch::Surfaces(surfaces_start..surfaces_end))
            }
        }
    }
}

#[derive(Debug)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
#[allow(missing_docs)]
pub enum PrimitiveBatch {
    Shadows(Range<usize>),
    Quads(Range<usize>),
    Paths(Range<usize>),
    Underlines(Range<usize>),
    MonochromeSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    SubpixelSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    PolychromeSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    Surfaces(Range<usize>),
}

impl PrimitiveBatch {
    #[expect(missing_docs)]
    pub fn label(&self) -> String {
        match self {
            Self::Shadows(range) => format!("shadows ({})", range.len()),
            Self::Quads(range) => format!("quads ({})", range.len()),
            Self::Paths(range) => format!("paths ({})", range.len()),
            Self::Underlines(range) => format!("underlines ({})", range.len()),
            Self::MonochromeSprites { texture_id, range } => {
                format!(
                    "monochrome sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::SubpixelSprites { texture_id, range } => {
                format!(
                    "subpixel sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::PolychromeSprites { texture_id, range } => {
                format!(
                    "polychrome sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::Surfaces(range) => format!("surfaces ({})", range.len()),
        }
    }
}

#[derive(Default, Debug, Copy, Clone)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Quad {
    pub order: DrawOrder,
    pub border_style: BorderStyle,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub background: Background,
    pub border_color: Hsla,
    pub corner_radii: Corners<ScaledPixels>,
    pub border_widths: Edges<ScaledPixels>,
}

impl From<Quad> for Primitive {
    fn from(quad: Quad) -> Self {
        Primitive::Quad(quad)
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Underline {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub thickness: ScaledPixels,
    pub wavy: PaddedBool32,
}

impl From<Underline> for Primitive {
    fn from(underline: Underline) -> Self {
        Primitive::Underline(underline)
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Shadow {
    pub order: DrawOrder,
    pub blur_radius: ScaledPixels,
    pub bounds: Bounds<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub shape_bounds: Bounds<ScaledPixels>,
    pub shape_corner_radii: Corners<ScaledPixels>,
    pub inset: PaddedBool32,
    pub pad: u32, // align to 8 bytes
}

impl From<Shadow> for Primitive {
    fn from(shadow: Shadow) -> Self {
        Primitive::Shadow(shadow)
    }
}

/// The style of a border.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[repr(C)]
pub enum BorderStyle {
    /// A solid border.
    #[default]
    Solid = 0,
    /// A dashed border.
    Dashed = 1,
}

/// A data type representing a 2 dimensional transformation that can be applied to an element.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct TransformationMatrix {
    /// 2x2 matrix containing rotation and scale,
    /// stored row-major
    pub rotation_scale: [[f32; 2]; 2],
    /// translation vector
    pub translation: [f32; 2],
}

impl Eq for TransformationMatrix {}

impl TransformationMatrix {
    /// The unit matrix, has no effect.
    pub fn unit() -> Self {
        Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [0.0, 0.0],
        }
    }

    /// Move the origin by a given point
    pub fn translate(self, point: Point<ScaledPixels>) -> Self {
        self.compose(Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [point.x.0, point.y.0],
        })
    }

    /// Clockwise rotation in radians around the origin
    pub fn rotate(self, angle: Radians) -> Self {
        self.compose(Self {
            rotation_scale: [
                [angle.0.cos(), -angle.0.sin()],
                [angle.0.sin(), angle.0.cos()],
            ],
            translation: [0.0, 0.0],
        })
    }

    /// Scale around the origin
    pub fn scale(self, size: Size<f32>) -> Self {
        self.compose(Self {
            rotation_scale: [[size.width, 0.0], [0.0, size.height]],
            translation: [0.0, 0.0],
        })
    }

    /// Perform matrix multiplication with another transformation
    /// to produce a new transformation that is the result of
    /// applying both transformations: first, `other`, then `self`.
    #[inline]
    pub fn compose(self, other: TransformationMatrix) -> TransformationMatrix {
        if other == Self::unit() {
            return self;
        }
        // Perform matrix multiplication
        TransformationMatrix {
            rotation_scale: [
                [
                    self.rotation_scale[0][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][0],
                    self.rotation_scale[0][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][1],
                ],
                [
                    self.rotation_scale[1][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][0],
                    self.rotation_scale[1][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][1],
                ],
            ],
            translation: [
                self.translation[0]
                    + self.rotation_scale[0][0] * other.translation[0]
                    + self.rotation_scale[0][1] * other.translation[1],
                self.translation[1]
                    + self.rotation_scale[1][0] * other.translation[0]
                    + self.rotation_scale[1][1] * other.translation[1],
            ],
        }
    }

    /// The smallest bounds that contain `bounds` once it is transformed.
    pub(crate) fn transform_bounds(&self, bounds: Bounds<ScaledPixels>) -> Bounds<ScaledPixels> {
        if *self == Self::unit() {
            return bounds;
        }
        let [[xx, xy], [yx, yy]] = self.rotation_scale;
        let [translate_x, translate_y] = self.translation;
        let corners = [
            bounds.origin,
            bounds.top_right(),
            bounds.bottom_left(),
            bounds.bottom_right(),
        ]
        .map(|corner| {
            Point::new(
                ScaledPixels(xx * corner.x.0 + xy * corner.y.0 + translate_x),
                ScaledPixels(yx * corner.x.0 + yy * corner.y.0 + translate_y),
            )
        });
        let [first, rest @ ..] = corners;
        let top_left = rest.iter().fold(first, |min, corner| min.min(corner));
        let bottom_right = rest.iter().fold(first, |max, corner| max.max(corner));
        Bounds::from_corners(top_left, bottom_right)
    }

    /// Apply transformation to a point, mainly useful for debugging
    pub fn apply(&self, point: Point<Pixels>) -> Point<Pixels> {
        let input = [point.x.0, point.y.0];
        let mut output = self.translation;
        for (i, output_cell) in output.iter_mut().enumerate() {
            for (k, input_cell) in input.iter().enumerate() {
                *output_cell += self.rotation_scale[i][k] * *input_cell;
            }
        }
        Point::new(output[0].into(), output[1].into())
    }
}

impl Default for TransformationMatrix {
    fn default() -> Self {
        Self::unit()
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct MonochromeSprite {
    pub order: DrawOrder,
    pub animation: SpriteAnimationId,
    pub tile: AtlasTile,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub transformation: TransformationMatrix,
}

/// Refers to an animation of the scene a sprite is inserted into.
///
/// A single `u32` so it fits the sprite's existing padding and leaves the
/// instance layout unchanged on every backend; renderers ignore it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct SpriteAnimationId(u32);

impl SpriteAnimationId {
    /// No animation.
    pub const NONE: Self = Self(u32::MAX);
}

impl Default for SpriteAnimationId {
    fn default() -> Self {
        Self::NONE
    }
}

/// A [`TimedTransformation`] the window applies to a sprite right before
/// every present, so the scene can be presented again without being redrawn.
#[derive(Copy, Clone, Debug)]
pub(crate) struct SpriteAnimation {
    transformation: TimedTransformation,
    center: Point<Pixels>,
    scale_factor: f32,
    base_color: Hsla,
    sprite_index: usize,
}

impl SpriteAnimation {
    pub(crate) fn new(
        transformation: TimedTransformation,
        center: Point<Pixels>,
        scale_factor: f32,
    ) -> Self {
        Self {
            transformation,
            center,
            scale_factor,
            base_color: Hsla::default(),
            sprite_index: usize::MAX,
        }
    }
}

impl From<MonochromeSprite> for Primitive {
    fn from(sprite: MonochromeSprite) -> Self {
        Primitive::MonochromeSprite(sprite)
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct SubpixelSprite {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub tile: AtlasTile,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub transformation: TransformationMatrix,
}

impl From<SubpixelSprite> for Primitive {
    fn from(sprite: SubpixelSprite) -> Self {
        Primitive::SubpixelSprite(sprite)
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct PolychromeSprite {
    pub order: DrawOrder,
    pub pad: u32,
    pub tile: AtlasTile,
    pub grayscale: PaddedBool32,
    pub opacity: f32,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
}

impl From<PolychromeSprite> for Primitive {
    fn from(sprite: PolychromeSprite) -> Self {
        Primitive::PolychromeSprite(sprite)
    }
}

#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct PaintSurface {
    pub order: DrawOrder,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub image_buffer: objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBuffer>,
}

impl From<PaintSurface> for Primitive {
    fn from(surface: PaintSurface) -> Self {
        Primitive::Surface(surface)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
#[expect(missing_docs)]
pub struct PathCurve {
    pub p0: Point<f32>,
    pub p1: Point<f32>,
    pub a: Point<f32>,
    pub b: Point<f32>,
    pub sign: f32,
    pub pad: u32, // align to 8 bytes
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct PathPaint {
    /// The path's bounds, in device pixels.
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Background,
    pub even_odd: PaddedBool32,
    pub pad: u32, // align to 8 bytes
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
#[expect(missing_docs)]
pub struct PathTile {
    pub paint: u32,
    pub curve_start: u32,
    pub curve_count: u32,
    pub backdrop: i32,
    pub top_left: Point<f32>,
    pub bottom_right: Point<f32>,
    pub reference: Point<f32>,
    pub curves_top_left: Point<f32>,
    pub curves_bottom_right: Point<f32>,
}

#[derive(Clone, Debug)]
#[expect(missing_docs)]
pub struct PaintedPath {
    order: DrawOrder,
    bounds: Bounds<ScaledPixels>,
    tiles: Rc<PathTiles>,
    paint: PathPaint,
}

#[expect(missing_docs)]
impl PaintedPath {
    /// Bins `path` into tiles, to be filled with `color` within `content_mask`.
    pub fn new(
        path: Path,
        scale_factor: f32,
        content_mask: ContentMask<Pixels>,
        color: Background,
    ) -> Self {
        let content_mask = content_mask.scale(scale_factor);
        let mut tiles = path.bin(scale_factor);
        tiles.retain_visible(content_mask.bounds);
        let paint = PathPaint {
            bounds: tiles.bounds(),
            content_mask,
            color,
            even_odd: path.even_odd().into(),
            pad: 0,
        };
        Self {
            order: 0,
            bounds: tiles.painted_bounds(),
            tiles: Rc::new(tiles),
            paint,
        }
    }

    pub fn tile_count(&self) -> usize {
        self.tiles.tiles().len()
    }

    /// Quiver: moved right by `dx` device pixels, clipped to `clip` (see
    /// [`Scene::pack_side_by_side`]).
    fn translated(&self, dx: f32, clip: &Bounds<ScaledPixels>) -> Self {
        let mv = |b: Bounds<ScaledPixels>| Bounds {
            origin: Point {
                x: ScaledPixels(b.origin.x.0 + dx),
                y: b.origin.y,
            },
            size: b.size,
        };
        let mut paint = self.paint;
        paint.bounds = mv(paint.bounds);
        paint.content_mask.bounds = mv(paint.content_mask.bounds).intersect(clip);
        Self {
            order: self.order,
            bounds: mv(self.bounds),
            tiles: Rc::new(self.tiles.translated(dx)),
            paint,
        }
    }

    pub fn append_to(
        &self,
        paints: &mut Vec<PathPaint>,
        curves: &mut Vec<PathCurve>,
        tiles: &mut Vec<PathTile>,
    ) {
        let paint_index = paints.len() as u32;
        let curve_start = curves.len() as u32;
        paints.push(self.paint);
        curves.extend_from_slice(self.tiles.curves());
        tiles.extend(self.tiles.tiles().iter().map(|tile| PathTile {
            paint: paint_index,
            curve_start: curve_start + tile.curve_start,
            ..*tile
        }));
    }
}

impl From<PaintedPath> for Primitive {
    fn from(path: PaintedPath) -> Self {
        Primitive::Path(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{point, size};

    fn square(x: f32) -> Bounds<ScaledPixels> {
        Bounds {
            origin: point(ScaledPixels(x), ScaledPixels(0.)),
            size: size(ScaledPixels(10.), ScaledPixels(10.)),
        }
    }

    fn quad(x: f32) -> Quad {
        Quad {
            bounds: square(x),
            content_mask: ContentMask { bounds: square(x) },
            ..Default::default()
        }
    }

    fn underline(x: f32) -> Underline {
        Underline {
            order: 0,
            pad: 0,
            bounds: square(x),
            content_mask: ContentMask { bounds: square(x) },
            color: Hsla::default(),
            thickness: ScaledPixels(1.),
            wavy: false.into(),
        }
    }

    fn batch_labels(scene: &mut Scene) -> Vec<String> {
        scene.finish();
        scene.batches().map(|batch| batch.label()).collect()
    }

    #[test]
    fn layers_are_drawn_in_paint_order() {
        // A quad painted after a layer goes above it.
        let mut scene = Scene::default();
        scene.insert_primitive(quad(0.));
        scene.push_layer();
        scene.insert_primitive(underline(0.));
        scene.insert_primitive(underline(5.));
        scene.pop_layer();
        scene.insert_primitive(quad(5.));
        assert_eq!(
            batch_labels(&mut scene),
            ["quads (1)", "underlines (2)", "quads (1)"]
        );

        // A layer goes above the layer painted before it.
        let mut scene = Scene::default();
        scene.push_layer();
        scene.insert_primitive(underline(0.));
        scene.pop_layer();
        scene.push_layer();
        scene.insert_primitive(quad(5.));
        scene.pop_layer();
        assert_eq!(batch_labels(&mut scene), ["underlines (1)", "quads (1)"]);
    }

    #[test]
    fn nested_layers_are_drawn_above_their_parents() {
        // A layer has one draw order, so what it paints after a nested layer
        // still goes below that layer.
        let mut scene = Scene::default();
        scene.push_layer();
        scene.insert_primitive(underline(0.));
        scene.push_layer();
        scene.insert_primitive(quad(5.));
        scene.pop_layer();
        scene.insert_primitive(underline(5.));
        scene.pop_layer();
        assert_eq!(batch_labels(&mut scene), ["underlines (2)", "quads (1)"]);

        // The same holds when the nested layer shares its parent's order.
        let mut scene = Scene::default();
        scene.push_layer();
        scene.insert_primitive(quad(0.));
        scene.push_layer();
        scene.insert_primitive(quad(5.));
        scene.pop_layer();
        scene.insert_primitive(quad(8.));
        scene.pop_layer();
        assert_eq!(batch_labels(&mut scene), ["quads (3)"]);
        assert_eq!(
            scene
                .quads
                .iter()
                .map(|quad| quad.bounds)
                .collect::<Vec<_>>(),
            [square(0.), square(8.), square(5.)]
        );

        // A layer goes above the layers nested in the layer before it.
        let mut scene = Scene::default();
        scene.push_layer();
        scene.push_layer();
        scene.insert_primitive(underline(0.));
        scene.pop_layer();
        scene.pop_layer();
        scene.push_layer();
        scene.insert_primitive(quad(5.));
        scene.pop_layer();
        assert_eq!(batch_labels(&mut scene), ["underlines (1)", "quads (1)"]);
    }
}

// Quiver: one eye's copy of each kind of primitive for a packed stereo frame
// ([`Scene::pack_side_by_side`]): moved right by `dx` device pixels, its content mask moved
// with it and clipped to `clip` (the eye).

fn moved(b: Bounds<ScaledPixels>, dx: f32) -> Bounds<ScaledPixels> {
    Bounds {
        origin: Point {
            x: ScaledPixels(b.origin.x.0 + dx),
            y: b.origin.y,
        },
        size: b.size,
    }
}

fn moved_mask(
    m: &ContentMask<ScaledPixels>,
    dx: f32,
    clip: &Bounds<ScaledPixels>,
) -> ContentMask<ScaledPixels> {
    ContentMask {
        bounds: moved(m.bounds, dx).intersect(clip),
    }
}

/// p' = R(p - s) + t + s: the same transform about the moved origin (row-major R, as
/// `transform_bounds` reads it).
fn moved_transform(t: TransformationMatrix, dx: f32) -> TransformationMatrix {
    let r = t.rotation_scale;
    TransformationMatrix {
        rotation_scale: r,
        translation: [
            t.translation[0] + dx - r[0][0] * dx,
            t.translation[1] - r[1][0] * dx,
        ],
    }
}

impl Shadow {
    fn translated(&self, dx: f32, clip: &Bounds<ScaledPixels>) -> Self {
        Shadow {
            bounds: moved(self.bounds, dx),
            shape_bounds: moved(self.shape_bounds, dx),
            content_mask: moved_mask(&self.content_mask, dx, clip),
            ..*self
        }
    }
}

impl Quad {
    fn translated(&self, dx: f32, clip: &Bounds<ScaledPixels>) -> Self {
        Quad {
            bounds: moved(self.bounds, dx),
            content_mask: moved_mask(&self.content_mask, dx, clip),
            ..*self
        }
    }
}

impl Underline {
    fn translated(&self, dx: f32, clip: &Bounds<ScaledPixels>) -> Self {
        Underline {
            bounds: moved(self.bounds, dx),
            content_mask: moved_mask(&self.content_mask, dx, clip),
            ..*self
        }
    }
}

impl MonochromeSprite {
    fn translated(&self, dx: f32, clip: &Bounds<ScaledPixels>) -> Self {
        MonochromeSprite {
            bounds: moved(self.bounds, dx),
            content_mask: moved_mask(&self.content_mask, dx, clip),
            transformation: moved_transform(self.transformation, dx),
            ..*self
        }
    }
}

impl SubpixelSprite {
    fn translated(&self, dx: f32, clip: &Bounds<ScaledPixels>) -> Self {
        SubpixelSprite {
            bounds: moved(self.bounds, dx),
            content_mask: moved_mask(&self.content_mask, dx, clip),
            transformation: moved_transform(self.transformation, dx),
            ..*self
        }
    }
}

impl PolychromeSprite {
    fn translated(&self, dx: f32, clip: &Bounds<ScaledPixels>) -> Self {
        PolychromeSprite {
            bounds: moved(self.bounds, dx),
            content_mask: moved_mask(&self.content_mask, dx, clip),
            ..*self
        }
    }
}

impl PaintSurface {
    fn translated(&self, dx: f32, clip: &Bounds<ScaledPixels>) -> Self {
        let mut p = self.clone();
        p.bounds = moved(p.bounds, dx);
        p.content_mask = moved_mask(&p.content_mask, dx, clip);
        p
    }
}
