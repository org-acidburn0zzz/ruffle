//! `MovieClip` display object and support code.
use crate::avm1::{Avm1, Object, StageObject, TObject, Value};
use crate::backend::audio::AudioStreamHandle;

use crate::avm1::activation::{Activation, ActivationIdentifier};
use crate::character::Character;
use crate::context::{ActionType, RenderContext, UpdateContext};
use crate::display_object::{
    Bitmap, Button, DisplayObjectBase, EditText, Graphic, MorphShapeStatic, TDisplayObject, Text,
};
use crate::drawing::Drawing;
use crate::events::{ButtonKeyCode, ClipEvent, ClipEventResult};
use crate::font::Font;
use crate::prelude::*;
use crate::shape_utils::DrawCommand;
use crate::tag_utils::{self, DecodeResult, SwfMovie, SwfSlice, SwfStream};
use enumset::{EnumSet, EnumSetType};
use gc_arena::{Collect, Gc, GcCell, MutationContext};
use smallvec::SmallVec;
use std::cell::Ref;
use std::collections::{BTreeMap, HashMap};
use std::convert::TryFrom;
use std::sync::Arc;
use swf::read::SwfRead;
use swf::{FillStyle, LineStyle};

type FrameNumber = u16;

/// A movie clip is a display object with its own timeline that runs independently of the root timeline.
/// The SWF19 spec calls this "Sprite" and the SWF tag defines it is "DefineSprite".
/// However, in AVM2, Sprite is a separate display object, and MovieClip is a subclass of Sprite.
///
/// (SWF19 pp. 201-203)
#[derive(Clone, Debug, Collect, Copy)]
#[collect(no_drop)]
pub struct MovieClip<'gc>(GcCell<'gc, MovieClipData<'gc>>);

#[derive(Clone, Debug)]
pub struct MovieClipData<'gc> {
    base: DisplayObjectBase<'gc>,
    static_data: Gc<'gc, MovieClipStatic>,
    tag_stream_pos: u64,
    current_frame: FrameNumber,
    audio_stream: Option<AudioStreamHandle>,
    children: BTreeMap<Depth, DisplayObject<'gc>>,
    object: Option<Object<'gc>>,
    clip_actions: Vec<ClipAction>,
    has_button_clip_event: bool,
    flags: EnumSet<MovieClipFlags>,
    avm1_constructor: Option<Object<'gc>>,
    drawing: Drawing,
}

impl<'gc> MovieClip<'gc> {
    #[allow(dead_code)]
    pub fn new(swf: SwfSlice, gc_context: MutationContext<'gc, '_>) -> Self {
        MovieClip(GcCell::allocate(
            gc_context,
            MovieClipData {
                base: Default::default(),
                static_data: Gc::allocate(gc_context, MovieClipStatic::empty(swf)),
                tag_stream_pos: 0,
                current_frame: 0,
                audio_stream: None,
                children: BTreeMap::new(),
                object: None,
                clip_actions: Vec::new(),
                has_button_clip_event: false,
                flags: EnumSet::empty(),
                avm1_constructor: None,
                drawing: Drawing::new(),
            },
        ))
    }

    pub fn new_with_data(
        gc_context: MutationContext<'gc, '_>,
        id: CharacterId,
        swf: SwfSlice,
        num_frames: u16,
    ) -> Self {
        MovieClip(GcCell::allocate(
            gc_context,
            MovieClipData {
                base: Default::default(),
                static_data: Gc::allocate(
                    gc_context,
                    MovieClipStatic {
                        id,
                        swf,
                        total_frames: num_frames,
                        audio_stream_info: None,
                        frame_labels: HashMap::new(),
                    },
                ),
                tag_stream_pos: 0,
                current_frame: 0,
                audio_stream: None,
                children: BTreeMap::new(),
                object: None,
                clip_actions: Vec::new(),
                has_button_clip_event: false,
                flags: MovieClipFlags::Playing.into(),
                avm1_constructor: None,
                drawing: Drawing::new(),
            },
        ))
    }

    /// Construct a movie clip that represents an entire movie.
    pub fn from_movie(gc_context: MutationContext<'gc, '_>, movie: Arc<SwfMovie>) -> Self {
        Self::new_with_data(
            gc_context,
            0,
            movie.clone().into(),
            movie.header().num_frames,
        )
    }

    /// Replace the current MovieClip with a completely new SwfMovie.
    ///
    /// Playback will start at position zero, any existing streamed audio will
    /// be terminated, and so on. Children and AVM data will be kept across the
    /// load boundary.
    pub fn replace_with_movie(
        &mut self,
        gc_context: MutationContext<'gc, '_>,
        movie: Option<Arc<SwfMovie>>,
    ) {
        self.0
            .write(gc_context)
            .replace_with_movie(gc_context, movie)
    }

    pub fn preload(
        self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        morph_shapes: &mut fnv::FnvHashMap<CharacterId, MorphShapeStatic>,
    ) {
        use swf::TagCode;
        // TODO: Re-creating static data because preload step occurs after construction.
        // Should be able to hoist this up somewhere, or use MaybeUninit.
        let mut static_data = (&*self.0.read().static_data).clone();
        let data = self.0.read().static_data.swf.clone();
        let mut reader = data.read_from(self.0.read().tag_stream_pos);
        let mut cur_frame = 1;
        let mut ids = fnv::FnvHashMap::default();
        let version = reader.version();
        let tag_callback = |reader: &mut SwfStream<&[u8]>, tag_code, tag_len| {
            let data = *reader.get_inner().get_ref();
            let tag_pos = reader.get_inner().position() as usize;
            let tag_slice = &data[tag_pos..tag_pos + tag_len];
            let reader = &mut SwfStream::new(std::io::Cursor::new(tag_slice), version);
            match tag_code {
                TagCode::FileAttributes => {
                    let attributes = reader.read_file_attributes()?;
                    if attributes.is_action_script_3 {
                        log::warn!("This SWF contains ActionScript 3 which is not yet supported by Ruffle. The movie may not work as intended.");
                    }
                    Ok(())
                }
                TagCode::DefineBits => self
                    .0
                    .write(context.gc_context)
                    .define_bits(context, reader, tag_len),
                TagCode::DefineBitsJpeg2 => self
                    .0
                    .write(context.gc_context)
                    .define_bits_jpeg_2(context, reader, tag_len),
                TagCode::DefineBitsJpeg3 => self
                    .0
                    .write(context.gc_context)
                    .define_bits_jpeg_3(context, reader, tag_len),
                TagCode::DefineBitsJpeg4 => self
                    .0
                    .write(context.gc_context)
                    .define_bits_jpeg_4(context, reader, tag_len),
                TagCode::DefineBitsLossless => self
                    .0
                    .write(context.gc_context)
                    .define_bits_lossless(context, reader, 1),
                TagCode::DefineBitsLossless2 => self
                    .0
                    .write(context.gc_context)
                    .define_bits_lossless(context, reader, 2),
                TagCode::DefineButton => self
                    .0
                    .write(context.gc_context)
                    .define_button_1(context, reader),
                TagCode::DefineButton2 => self
                    .0
                    .write(context.gc_context)
                    .define_button_2(context, reader),
                TagCode::DefineButtonCxform => self
                    .0
                    .write(context.gc_context)
                    .define_button_cxform(context, reader, tag_len),
                TagCode::DefineButtonSound => self
                    .0
                    .write(context.gc_context)
                    .define_button_sound(context, reader),
                TagCode::DefineEditText => self
                    .0
                    .write(context.gc_context)
                    .define_edit_text(context, reader),
                TagCode::DefineFont => self
                    .0
                    .write(context.gc_context)
                    .define_font_1(context, reader),
                TagCode::DefineFont2 => self
                    .0
                    .write(context.gc_context)
                    .define_font_2(context, reader),
                TagCode::DefineFont3 => self
                    .0
                    .write(context.gc_context)
                    .define_font_3(context, reader),
                TagCode::DefineFont4 => unimplemented!(),
                TagCode::DefineMorphShape => self.0.write(context.gc_context).define_morph_shape(
                    context,
                    reader,
                    morph_shapes,
                    1,
                ),
                TagCode::DefineMorphShape2 => self.0.write(context.gc_context).define_morph_shape(
                    context,
                    reader,
                    morph_shapes,
                    2,
                ),
                TagCode::DefineShape => self
                    .0
                    .write(context.gc_context)
                    .define_shape(context, reader, 1),
                TagCode::DefineShape2 => self
                    .0
                    .write(context.gc_context)
                    .define_shape(context, reader, 2),
                TagCode::DefineShape3 => self
                    .0
                    .write(context.gc_context)
                    .define_shape(context, reader, 3),
                TagCode::DefineShape4 => self
                    .0
                    .write(context.gc_context)
                    .define_shape(context, reader, 4),
                TagCode::DefineSound => self
                    .0
                    .write(context.gc_context)
                    .define_sound(context, reader),
                TagCode::DefineSprite => self.0.write(context.gc_context).define_sprite(
                    context,
                    reader,
                    tag_len,
                    morph_shapes,
                ),
                TagCode::DefineText => self
                    .0
                    .write(context.gc_context)
                    .define_text(context, reader, 1),
                TagCode::DefineText2 => self
                    .0
                    .write(context.gc_context)
                    .define_text(context, reader, 2),
                TagCode::DoInitAction => self.do_init_action(context, reader, tag_len),
                TagCode::DoAbc => self.do_abc(context, reader, tag_len),
                TagCode::ExportAssets => self
                    .0
                    .write(context.gc_context)
                    .export_assets(context, reader),
                TagCode::FrameLabel => self.0.write(context.gc_context).frame_label(
                    context,
                    reader,
                    tag_len,
                    cur_frame,
                    &mut static_data,
                ),
                TagCode::JpegTables => self
                    .0
                    .write(context.gc_context)
                    .jpeg_tables(context, reader, tag_len),
                TagCode::PlaceObject => self.0.write(context.gc_context).preload_place_object(
                    context,
                    reader,
                    tag_len,
                    &mut ids,
                    morph_shapes,
                    1,
                ),
                TagCode::PlaceObject2 => self.0.write(context.gc_context).preload_place_object(
                    context,
                    reader,
                    tag_len,
                    &mut ids,
                    morph_shapes,
                    2,
                ),
                TagCode::PlaceObject3 => self.0.write(context.gc_context).preload_place_object(
                    context,
                    reader,
                    tag_len,
                    &mut ids,
                    morph_shapes,
                    3,
                ),
                TagCode::PlaceObject4 => self.0.write(context.gc_context).preload_place_object(
                    context,
                    reader,
                    tag_len,
                    &mut ids,
                    morph_shapes,
                    4,
                ),
                TagCode::RemoveObject => self
                    .0
                    .write(context.gc_context)
                    .preload_remove_object(context, reader, &mut ids, 1),
                TagCode::RemoveObject2 => self
                    .0
                    .write(context.gc_context)
                    .preload_remove_object(context, reader, &mut ids, 2),
                TagCode::ShowFrame => self.0.write(context.gc_context).preload_show_frame(
                    context,
                    reader,
                    &mut cur_frame,
                ),
                TagCode::ScriptLimits => self
                    .0
                    .write(context.gc_context)
                    .script_limits(reader, context.avm1),
                TagCode::SoundStreamHead => self
                    .0
                    .write(context.gc_context)
                    .preload_sound_stream_head(context, reader, cur_frame, &mut static_data, 1),
                TagCode::SoundStreamHead2 => self
                    .0
                    .write(context.gc_context)
                    .preload_sound_stream_head(context, reader, cur_frame, &mut static_data, 2),
                TagCode::SoundStreamBlock => {
                    self.0.write(context.gc_context).preload_sound_stream_block(
                        context,
                        reader,
                        cur_frame,
                        &mut static_data,
                        tag_len,
                    )
                }
                _ => Ok(()),
            }
        };
        let _ = tag_utils::decode_tags(&mut reader, tag_callback, TagCode::End);
        self.0.write(context.gc_context).static_data =
            Gc::allocate(context.gc_context, static_data);

        // Finalize audio stream.
        if self.0.read().static_data.audio_stream_info.is_some() {
            context.audio.preload_sound_stream_end(self.0.read().id());
        }
    }

    #[inline]
    fn do_init_action(
        self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&[u8]>,
        tag_len: usize,
    ) -> DecodeResult {
        // Queue the init actions.

        // TODO: Init actions are supposed to be executed once, and it gives a
        // sprite ID... how does that work?
        let sprite_id = reader.read_u16()?;
        log::info!("Init Action sprite ID {}", sprite_id);

        let slice = self
            .0
            .read()
            .static_data
            .swf
            .resize_to_reader(reader, tag_len)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Invalid source or tag length when running init action",
                )
            })?;

        Avm1::run_stack_frame_for_init_action(
            self.into(),
            context.swf.header().version,
            slice,
            context,
        );

        Ok(())
    }

    #[inline]
    fn do_abc(
        self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&[u8]>,
        tag_len: usize,
    ) -> DecodeResult {
        // Queue the actions.
        // TODO: The tag reader parses the entire ABC file, instead of just
        // giving us a `SwfSlice` for later parsing, so we have to replcate the
        // *entire* parsing code here. This sucks.
        let flags = reader.read_u32()?;
        let name = reader.read_c_string()?;
        let is_lazy_initialize = flags & 1 != 0;

        // The rest of the tag is an ABC file so we can take our SwfSlice now.
        let slice = self
            .0
            .read()
            .static_data
            .swf
            .resize_to_reader(reader, tag_len)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Invalid source or tag length when running init action",
                )
            })?;
        context.action_queue.queue_actions(
            self.into(),
            ActionType::DoABC {
                name,
                is_lazy_initialize,
                abc: slice,
            },
            false,
        );
        Ok(())
    }

    #[allow(dead_code)]
    pub fn playing(self) -> bool {
        self.0.read().playing()
    }

    pub fn next_frame(self, context: &mut UpdateContext<'_, 'gc, '_>) {
        if self.current_frame() < self.total_frames() {
            self.goto_frame(context, self.current_frame() + 1, true);
        }
    }

    pub fn play(self, context: &mut UpdateContext<'_, 'gc, '_>) {
        self.0.write(context.gc_context).play()
    }

    pub fn prev_frame(self, context: &mut UpdateContext<'_, 'gc, '_>) {
        if self.current_frame() > 1 {
            self.goto_frame(context, self.current_frame() - 1, true);
        }
    }

    pub fn stop(self, context: &mut UpdateContext<'_, 'gc, '_>) {
        self.0.write(context.gc_context).stop(context)
    }

    /// Queues up a goto to the specified frame.
    /// `frame` should be 1-based.
    pub fn goto_frame(
        self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        mut frame: FrameNumber,
        stop: bool,
    ) {
        // Stop first, in case we need to kill and restart the stream sound.
        if stop {
            self.stop(context);
        } else {
            self.play(context);
        }

        // Clamp frame number in bounds.
        if frame < 1 {
            frame = 1;
        }

        if frame != self.current_frame() {
            self.run_goto(self.into(), context, frame);
        }
    }

    pub fn current_frame(self) -> FrameNumber {
        self.0.read().current_frame
    }

    pub fn total_frames(self) -> FrameNumber {
        self.0.read().static_data.total_frames
    }

    pub fn frames_loaded(self) -> FrameNumber {
        // TODO(Herschel): root needs to progressively stream in frames.
        self.0.read().static_data.total_frames
    }

    pub fn set_avm1_constructor(
        self,
        gc_context: MutationContext<'gc, '_>,
        prototype: Option<Object<'gc>>,
    ) {
        self.0.write(gc_context).avm1_constructor = prototype;
    }

    pub fn frame_label_to_number(self, frame_label: &str) -> Option<FrameNumber> {
        // Frame labels are case insensitive.
        let label = frame_label.to_ascii_lowercase();
        self.0.read().static_data.frame_labels.get(&label).copied()
    }

    /// Returns the highest depth in use by this movie clip, or `None` if there are no children.
    pub fn highest_depth(self) -> Option<Depth> {
        self.0.read().children.keys().copied().rev().next()
    }

    /// Gets the clip events for this movieclip.
    pub fn clip_actions(&self) -> Ref<[ClipAction]> {
        Ref::map(self.0.read(), |mc| mc.clip_actions())
    }

    /// Sets the clip actions (a.k.a. clip events) for this movieclip.
    /// Clip actions are created in the Flash IDE by using the `onEnterFrame`
    /// tag on a movieclip instance.
    pub fn set_clip_actions(self, gc_context: MutationContext<'gc, '_>, actions: Vec<ClipAction>) {
        let mut mc = self.0.write(gc_context);
        mc.has_button_clip_event = actions.iter().any(|a| a.event.is_button_event());
        mc.set_clip_actions(actions);
    }

    /// Adds a script-created display object as a child to this clip.
    pub fn add_child_from_avm(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        mut child: DisplayObject<'gc>,
        depth: Depth,
    ) {
        let mut parent = self.0.write(context.gc_context);

        let prev_child = parent.children.insert(depth, child);
        if let Some(prev_child) = prev_child {
            parent.remove_child_from_exec_list(context, prev_child);
        }
        parent.add_child_to_exec_list(context.gc_context, child);
        child.set_parent(context.gc_context, Some((*self).into()));
        child.set_place_frame(context.gc_context, 0);
        child.set_depth(context.gc_context, depth);
    }

    /// Remove a child from this clip.
    pub fn remove_child_from_avm(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        child: DisplayObject<'gc>,
    ) {
        debug_assert!(DisplayObject::ptr_eq(
            child.parent().unwrap(),
            (*self).into()
        ));
        let mut parent = self.0.write(context.gc_context);
        if let Some(child) = parent.children.remove(&child.depth()) {
            parent.remove_child_from_exec_list(context, child);
        }
    }

    /// Swaps a child to a target depth.
    pub fn swap_child_to_depth(
        self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        child: DisplayObject<'gc>,
        depth: Depth,
    ) {
        // Verify this is actually our child.
        debug_assert!(DisplayObject::ptr_eq(child.parent().unwrap(), self.into()));

        // TODO: It'd be nice to just do a swap here, but no swap functionality in BTreeMap.
        let mut parent = self.0.write(context.gc_context);
        let prev_depth = child.depth();
        child.set_depth(context.gc_context, depth);
        child.set_transformed_by_script(context.gc_context, true);
        if let Some(prev_child) = parent.children.insert(depth, child) {
            prev_child.set_depth(context.gc_context, prev_depth);
            prev_child.set_transformed_by_script(context.gc_context, true);
            parent.children.insert(prev_depth, prev_child);
        } else {
            parent.children.remove(&prev_depth);
        }
    }

    /// Returns an iterator of AVM1 `DoAction` blocks on the given frame number.
    /// Used by the AVM `Call` action.
    pub fn actions_on_frame(
        self,
        _context: &mut UpdateContext<'_, 'gc, '_>,
        frame: FrameNumber,
    ) -> impl DoubleEndedIterator<Item = SwfSlice> {
        use swf::{read::Reader, TagCode};

        let mut actions: SmallVec<[SwfSlice; 2]> = SmallVec::new();

        // Iterate through this clip's tags, counting frames until we reach the target frame.
        if frame > 0 && frame <= self.total_frames() {
            let mut cur_frame = 1;
            let clip = self.0.read();
            let len = clip.tag_stream_len();
            let mut reader = clip.static_data.swf.read_from(0);
            while cur_frame <= frame && reader.get_ref().position() < len as u64 {
                let tag_callback =
                    |reader: &mut Reader<std::io::Cursor<&[u8]>>, tag_code, tag_len| {
                        match tag_code {
                            TagCode::ShowFrame => cur_frame += 1,
                            TagCode::DoAction if cur_frame == frame => {
                                // On the target frame, add any DoAction tags to the array.
                                if let Some(code) =
                                    clip.static_data.swf.resize_to_reader(reader, tag_len)
                                {
                                    actions.push(code)
                                }
                            }
                            _ => (),
                        }
                        Ok(())
                    };

                let _ = tag_utils::decode_tags(&mut reader, tag_callback, TagCode::ShowFrame);
            }
        }

        actions.into_iter()
    }

    pub fn set_fill_style(
        self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        style: Option<FillStyle>,
    ) {
        let mut mc = self.0.write(context.gc_context);
        mc.drawing.set_fill_style(style);
    }

    pub fn clear(self, context: &mut UpdateContext<'_, 'gc, '_>) {
        let mut mc = self.0.write(context.gc_context);
        mc.drawing.clear();
    }

    pub fn set_line_style(
        self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        style: Option<LineStyle>,
    ) {
        let mut mc = self.0.write(context.gc_context);
        mc.drawing.set_line_style(style);
    }

    pub fn draw_command(self, context: &mut UpdateContext<'_, 'gc, '_>, command: DrawCommand) {
        let mut mc = self.0.write(context.gc_context);
        mc.drawing.draw_command(command);
    }

    pub fn run_clip_event(
        self,
        context: &mut crate::context::UpdateContext<'_, 'gc, '_>,
        event: ClipEvent,
    ) {
        self.0
            .write(context.gc_context)
            .run_clip_event(self.into(), context, event);
    }

    fn run_frame_internal(
        self,
        self_display_object: DisplayObject<'gc>,
        context: &mut UpdateContext<'_, 'gc, '_>,
        run_display_actions: bool,
    ) {
        // Advance frame number.
        if self.current_frame() < self.total_frames() {
            self.0.write(context.gc_context).current_frame += 1;
        } else if self.total_frames() > 1 {
            // Looping acts exactly like a gotoAndPlay(1).
            // Specifically, object that existed on frame 1 should not be destroyed
            // and recreated.
            self.run_goto(self_display_object, context, 1);
            return;
        } else {
            // Single frame clips do not play.
            self.stop(context);
        }

        let mc = self.0.read();
        let _tag_pos = mc.tag_stream_pos;
        let data = mc.static_data.swf.clone();
        let mut reader = data.read_from(mc.tag_stream_pos);
        let mut has_stream_block = false;
        drop(mc);

        let version = reader.version();
        use swf::TagCode;
        let tag_callback = |reader: &mut SwfStream<&[u8]>, tag_code, tag_len| {
            let data = *reader.get_inner().get_ref();
            let tag_pos = reader.get_inner().position() as usize;
            let tag_slice = &data[tag_pos..tag_pos + tag_len];
            let reader = &mut SwfStream::new(std::io::Cursor::new(tag_slice), version);
            match tag_code {
                TagCode::DoAction => self.do_action(self_display_object, context, reader, tag_len),
                TagCode::PlaceObject if run_display_actions => {
                    self.place_object(self_display_object, context, reader, tag_len, 1)
                }
                TagCode::PlaceObject2 if run_display_actions => {
                    self.place_object(self_display_object, context, reader, tag_len, 2)
                }
                TagCode::PlaceObject3 if run_display_actions => {
                    self.place_object(self_display_object, context, reader, tag_len, 3)
                }
                TagCode::PlaceObject4 if run_display_actions => {
                    self.place_object(self_display_object, context, reader, tag_len, 4)
                }
                TagCode::RemoveObject if run_display_actions => {
                    self.remove_object(context, reader, 1)
                }
                TagCode::RemoveObject2 if run_display_actions => {
                    self.remove_object(context, reader, 2)
                }
                TagCode::SetBackgroundColor => self.set_background_color(context, reader),
                TagCode::StartSound => self.start_sound_1(context, reader),
                TagCode::SoundStreamBlock => {
                    has_stream_block = true;
                    self.sound_stream_block(context, reader)
                }
                _ => Ok(()),
            }
        };
        let _ = tag_utils::decode_tags(&mut reader, tag_callback, TagCode::ShowFrame);

        self.0.write(context.gc_context).tag_stream_pos = reader.get_ref().position();

        // If we are playing a streaming sound, there should(?) be a `SoundStreamBlock` on each frame.
        if !has_stream_block {
            self.0.write(context.gc_context).stop_audio_stream(context);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn instantiate_child(
        self,
        self_display_object: DisplayObject<'gc>,
        context: &mut UpdateContext<'_, 'gc, '_>,
        id: CharacterId,
        depth: Depth,
        place_object: &swf::PlaceObject,
        copy_previous_properties: bool,
    ) -> Option<DisplayObject<'gc>> {
        if let Ok(mut child) = context
            .library
            .library_for_movie_mut(self.movie().unwrap()) //TODO
            .instantiate_by_id(id, context.gc_context)
        {
            // Remove previous child from children list,
            // and add new childonto front of the list.
            let prev_child = {
                let mut mc = self.0.write(context.gc_context);
                let prev_child = mc.children.insert(depth, child);
                if let Some(prev_child) = prev_child {
                    mc.remove_child_from_exec_list(context, prev_child);
                }
                mc.add_child_to_exec_list(context.gc_context, child);
                prev_child
            };
            {
                // Set initial properties for child.
                child.set_depth(context.gc_context, depth);
                child.set_parent(context.gc_context, Some(self_display_object));
                child.set_place_frame(context.gc_context, self.current_frame());
                if copy_previous_properties {
                    if let Some(prev_child) = prev_child {
                        child.copy_display_properties_from(context.gc_context, prev_child);
                    }
                }
                // Run first frame.
                child.apply_place_object(context.gc_context, place_object);
                child.post_instantiation(context, child, None, false);
                child.run_frame(context);
            }
            Some(child)
        } else {
            log::error!("Unable to instantiate display node id {}", id);
            None
        }
    }

    pub fn run_goto(
        self,
        self_display_object: DisplayObject<'gc>,
        context: &mut UpdateContext<'_, 'gc, '_>,
        frame: FrameNumber,
    ) {
        // Flash gotos are tricky:
        // 1) Conceptually, a goto should act like the playhead is advancing forward or
        //    backward to a frame.
        // 2) However, MovieClip timelines are stored as deltas from frame to frame,
        //    so for rewinds, we must restart to frame 1 and play forward.
        // 3) Objects that would persist over the goto conceptually should not be
        //    destroyed and recreated; they should keep their properties.
        //    Particularly for rewinds, the object should persist if it was created
        //      *before* the frame we are going to. (DisplayObject::place_frame).
        // 4) We want to avoid creating objects just to destroy them if they aren't on
        //    the goto frame, so we should instead aggregate the deltas into a final list
        //    of commands, and THEN modify the children as necessary.

        // This map will maintain a map of depth -> placement commands.
        // TODO: Move this to UpdateContext to avoid allocations.
        let mut goto_commands = vec![];

        self.0.write(context.gc_context).stop_audio_stream(context);

        let is_rewind = if frame < self.current_frame() {
            // Because we can only step forward, we have to start at frame 1
            // when rewinding.
            self.0.write(context.gc_context).tag_stream_pos = 0;
            self.0.write(context.gc_context).current_frame = 0;

            // Remove all display objects that were created after the desination frame.
            // TODO: We want to do something like self.children.retain here,
            // but BTreeMap::retain does not exist.
            let children: SmallVec<[_; 16]> = self
                .0
                .read()
                .children
                .iter()
                .filter_map(|(depth, clip)| {
                    if clip.place_frame() > frame {
                        Some((*depth, *clip))
                    } else {
                        None
                    }
                })
                .collect();
            for (depth, child) in children {
                let mut mc = self.0.write(context.gc_context);
                mc.children.remove(&depth);
                mc.remove_child_from_exec_list(context, child);
            }
            true
        } else {
            false
        };

        // Step through the intermediate frames, and aggregate the deltas of each frame.
        let mc = self.0.read();
        let mut frame_pos = mc.tag_stream_pos;
        let data = mc.static_data.swf.clone();
        let mut reader = data.read_from(mc.tag_stream_pos);
        let mut index = 0;

        let len = mc.tag_stream_len() as u64;
        // Sanity; let's make sure we don't seek way too far.
        // TODO: This should be self.frames_loaded() when we implement that.
        let clamped_frame = if frame <= mc.total_frames() {
            frame
        } else {
            mc.total_frames()
        };
        drop(mc);

        while self.current_frame() < clamped_frame && frame_pos < len {
            self.0.write(context.gc_context).current_frame += 1;
            frame_pos = reader.get_inner().position();

            let mut mc = self.0.write(context.gc_context);
            let version = reader.version();
            use swf::TagCode;
            let tag_callback = |reader: &mut SwfStream<&[u8]>, tag_code, tag_len| {
                let data = *reader.get_inner().get_ref();
                let tag_pos = reader.get_inner().position() as usize;
                let tag_slice = &data[tag_pos..tag_pos + tag_len];
                let reader = &mut SwfStream::new(std::io::Cursor::new(tag_slice), version);
                match tag_code {
                    TagCode::PlaceObject => {
                        index += 1;
                        mc.goto_place_object(
                            reader,
                            tag_len,
                            1,
                            &mut goto_commands,
                            is_rewind,
                            index,
                        )
                    }
                    TagCode::PlaceObject2 => {
                        index += 1;
                        mc.goto_place_object(
                            reader,
                            tag_len,
                            2,
                            &mut goto_commands,
                            is_rewind,
                            index,
                        )
                    }
                    TagCode::PlaceObject3 => {
                        index += 1;
                        mc.goto_place_object(
                            reader,
                            tag_len,
                            3,
                            &mut goto_commands,
                            is_rewind,
                            index,
                        )
                    }
                    TagCode::PlaceObject4 => {
                        index += 1;
                        mc.goto_place_object(
                            reader,
                            tag_len,
                            4,
                            &mut goto_commands,
                            is_rewind,
                            index,
                        )
                    }
                    TagCode::RemoveObject => {
                        mc.goto_remove_object(reader, 1, context, &mut goto_commands, is_rewind)
                    }
                    TagCode::RemoveObject2 => {
                        mc.goto_remove_object(reader, 2, context, &mut goto_commands, is_rewind)
                    }
                    _ => Ok(()),
                }
            };
            let _ = tag_utils::decode_tags(&mut reader, tag_callback, TagCode::ShowFrame);
        }
        let hit_target_frame = self.0.read().current_frame == frame;

        // Run the list of goto commands to actually create and update the display objects.
        let run_goto_command = |clip: MovieClip<'gc>,
                                context: &mut UpdateContext<'_, 'gc, '_>,
                                params: &GotoPlaceObject| {
            let child_entry = clip.0.read().children.get(&params.depth()).copied();
            match child_entry {
                // Apply final delta to display pamareters.
                // For rewinds, if an object was created before the final frame,
                // it will exist on the final frame as well. Re-use this object
                // instead of recreating.
                // If the ID is 0, we are modifying a previous child. Otherwise, we're replacing it.
                // If it's a rewind, we removed any dead children above, so we always
                // modify the previous child.
                Some(mut prev_child) if params.id() == 0 || is_rewind => {
                    prev_child.apply_place_object(context.gc_context, &params.place_object);
                }
                _ => {
                    if let Some(mut child) = clip.instantiate_child(
                        self_display_object,
                        context,
                        params.id(),
                        params.depth(),
                        &params.place_object,
                        params.modifies_original_item(),
                    ) {
                        // Set the place frame to the frame where the object *would* have been placed.
                        child.set_place_frame(context.gc_context, params.frame);
                    }
                }
            }
        };

        // We have to be sure that queued actions are generated in the same order
        // as if the playhead had reached this frame normally.

        // First, sort the goto commands in the order of execution.
        // (Maybe it'd be better to keeps this list sorted as we create it?
        // Currently `swap_remove` calls futz with the order; but we could use `remove`).
        goto_commands.sort_by_key(|params| params.index);

        // Then, run frames for children that were created before this frame.
        goto_commands
            .iter()
            .filter(|params| params.frame < frame)
            .for_each(|goto| run_goto_command(self, context, goto));

        // Next, run the final frame for the parent clip.
        // Re-run the final frame without display tags (DoAction, StartSound, etc.)
        // Note that this only happens if the frame exists and is loaded;
        // e.g. gotoAndStop(9999) displays the final frame, but actions don't run!
        if hit_target_frame {
            self.0.write(context.gc_context).current_frame -= 1;
            self.0.write(context.gc_context).tag_stream_pos = frame_pos;
            self.run_frame_internal(self_display_object, context, false);
        } else {
            self.0.write(context.gc_context).current_frame = clamped_frame;
        }

        // Finally, run frames for children that are placed on this frame.
        goto_commands
            .iter()
            .filter(|params| params.frame >= frame)
            .for_each(|goto| run_goto_command(self, context, goto));
    }
}

impl<'gc> TDisplayObject<'gc> for MovieClip<'gc> {
    impl_display_object!(base);

    fn id(&self) -> CharacterId {
        self.0.read().id()
    }

    fn movie(&self) -> Option<Arc<SwfMovie>> {
        Some(self.0.read().movie())
    }

    fn run_frame(&mut self, context: &mut UpdateContext<'_, 'gc, '_>) {
        // Children must run first.
        for mut child in self.children() {
            child.run_frame(context);
        }

        // Run my load/enterFrame clip event.
        let mut mc = self.0.write(context.gc_context);
        let is_load_frame = !mc.initialized();
        if is_load_frame {
            mc.run_clip_event((*self).into(), context, ClipEvent::Load);
            mc.set_initialized(true);
        } else {
            mc.run_clip_event((*self).into(), context, ClipEvent::EnterFrame);
        }
        drop(mc);

        // Run my SWF tags.
        if self.playing() {
            self.run_frame_internal((*self).into(), context, true);
        }

        if is_load_frame {
            self.0.write(context.gc_context).run_clip_postevent(
                (*self).into(),
                context,
                ClipEvent::Load,
            );
        }
    }

    fn render(&self, context: &mut RenderContext<'_, 'gc>) {
        context.transform_stack.push(&*self.transform());
        crate::display_object::render_children(context, &self.0.read().children);
        self.0.read().drawing.render(context);
        context.transform_stack.pop();
    }

    fn self_bounds(&self) -> BoundingBox {
        self.0.read().drawing.self_bounds()
    }

    fn hit_test(&self, point: (Twips, Twips)) -> bool {
        self.world_bounds().contains(point)
    }

    fn mouse_pick(
        &self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        self_node: DisplayObject<'gc>,
        point: (Twips, Twips),
    ) -> Option<DisplayObject<'gc>> {
        if self.visible() {
            if self.world_bounds().contains(point) {
                if self.0.read().has_button_clip_event {
                    return Some(self_node);
                }

                let mut activation = Activation::from_stub(
                    context.reborrow(),
                    ActivationIdentifier::root("[Mouse Pick]"),
                );
                let object = self.object().coerce_to_object(&mut activation);

                if ClipEvent::BUTTON_EVENT_METHODS
                    .iter()
                    .any(|handler| object.has_property(&mut activation, handler))
                {
                    return Some(self_node);
                }
            }

            // Maybe we could skip recursing down at all if !world_bounds.contains(point),
            // but a child button can have an invisible hit area outside the parent's bounds.
            for child in self.0.read().children.values().rev() {
                let result = child.mouse_pick(context, *child, point);
                if result.is_some() {
                    return result;
                }
            }
        }

        None
    }

    fn handle_clip_event(
        &self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        event: ClipEvent,
    ) -> ClipEventResult {
        if event.propagates() {
            for child in self.children() {
                if child.handle_clip_event(context, event) == ClipEventResult::Handled {
                    return ClipEventResult::Handled;
                }
            }
        }

        self.0.read().run_clip_event((*self).into(), context, event)
    }

    fn as_movie_clip(&self) -> Option<MovieClip<'gc>> {
        Some(*self)
    }

    fn post_instantiation(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        display_object: DisplayObject<'gc>,
        init_object: Option<Object<'gc>>,
        instantiated_from_avm: bool,
    ) {
        self.set_default_instance_name(context);

        if self.0.read().object.is_none() {
            let version = context.swf.version();
            let globals = context.avm1.global_object_cell();

            // If we are running within the AVM, this must be an immediate action.
            // If we are not, then this must be queued to be ran first-thing
            if instantiated_from_avm && self.0.read().avm1_constructor.is_some() {
                let mut activation = Activation::from_nothing(
                    context.reborrow(),
                    ActivationIdentifier::root("[Construct]"),
                    version,
                    globals,
                    (*self).into(),
                );

                let constructor = self.0.read().avm1_constructor.unwrap();
                if let Ok(prototype) = constructor
                    .get("prototype", &mut activation)
                    .map(|v| v.coerce_to_object(&mut activation))
                {
                    let object: Object<'gc> = StageObject::for_display_object(
                        activation.context.gc_context,
                        (*self).into(),
                        Some(prototype),
                    )
                    .into();
                    if let Some(init_object) = init_object {
                        for key in init_object.get_keys(&mut activation) {
                            if let Ok(value) = init_object.get(&key, &mut activation) {
                                let _ = object.set(&key, value, &mut activation);
                            }
                        }
                    }
                    self.0.write(activation.context.gc_context).object = Some(object);
                    let _ = constructor.construct_on_existing(&mut activation, object, &[]);
                }

                return;
            }

            let object = StageObject::for_display_object(
                context.gc_context,
                display_object,
                Some(context.system_prototypes.movie_clip),
            );
            if let Some(init_object) = init_object {
                let mut activation = Activation::from_nothing(
                    context.reborrow(),
                    ActivationIdentifier::root("[Init]"),
                    version,
                    globals,
                    (*self).into(),
                );

                for key in init_object.get_keys(&mut activation) {
                    if let Ok(value) = init_object.get(&key, &mut activation) {
                        let _ = object.set(&key, value, &mut activation);
                    }
                }
            }
            let mut mc = self.0.write(context.gc_context);
            mc.object = Some(object.into());

            let mut events = Vec::new();

            for clip_action in mc
                .clip_actions()
                .iter()
                .filter(|action| action.event == ClipEvent::Construct)
            {
                events.push(clip_action.action_data.clone());
            }

            context.action_queue.queue_actions(
                display_object,
                ActionType::Construct {
                    constructor: mc.avm1_constructor,
                    events,
                },
                false,
            );
        }

        // If this text field has a variable set, initialize text field binding.
        Avm1::run_with_stack_frame_for_display_object(
            (*self).into(),
            context.swf.version(),
            context,
            |activation| {
                self.bind_text_field_variables(activation);
            },
        );
    }

    fn object(&self) -> Value<'gc> {
        self.0
            .read()
            .object
            .map(Value::from)
            .unwrap_or(Value::Undefined)
    }

    fn unload(&mut self, context: &mut UpdateContext<'_, 'gc, '_>) {
        for mut child in self.children() {
            child.unload(context);
        }

        // Unregister any text field variable bindings.
        if let Value::Object(object) = self.object() {
            if let Some(stage_object) = object.as_stage_object() {
                stage_object.unregister_text_field_bindings(context);
            }
        }

        {
            let mut mc = self.0.write(context.gc_context);
            mc.stop_audio_stream(context);
            mc.run_clip_event((*self).into(), context, ClipEvent::Unload);
        }
        self.set_removed(context.gc_context, true);
    }

    fn allow_as_mask(&self) -> bool {
        !self.0.read().children.is_empty()
    }

    fn get_child_by_name(&self, name: &str, case_sensitive: bool) -> Option<DisplayObject<'gc>> {
        crate::display_object::get_child_by_name(&self.0.read().children, name, case_sensitive)
    }
}

unsafe impl<'gc> Collect for MovieClipData<'gc> {
    #[inline]
    fn trace(&self, cc: gc_arena::CollectionContext) {
        for child in self.children.values() {
            child.trace(cc);
        }
        self.base.trace(cc);
        self.static_data.trace(cc);
        self.object.trace(cc);
        self.avm1_constructor.trace(cc);
    }
}

impl<'gc> MovieClipData<'gc> {
    /// Replace the current MovieClipData with a completely new SwfMovie.
    ///
    /// Playback will start at position zero, any existing streamed audio will
    /// be terminated, and so on. Children and AVM data will NOT be kept across
    /// the load boundary.
    ///
    /// If no movie is provided, then the movie clip will be replaced with an
    /// empty movie of the same SWF version.
    pub fn replace_with_movie(
        &mut self,
        gc_context: MutationContext<'gc, '_>,
        movie: Option<Arc<SwfMovie>>,
    ) {
        let movie = movie.unwrap_or_else(|| Arc::new(SwfMovie::empty(self.movie().version())));
        let total_frames = movie.header().num_frames;

        self.base.reset_for_movie_load();
        self.static_data = Gc::allocate(
            gc_context,
            MovieClipStatic {
                id: 0,
                swf: movie.into(),
                total_frames,
                audio_stream_info: None,
                frame_labels: HashMap::new(),
            },
        );
        self.tag_stream_pos = 0;
        self.flags = MovieClipFlags::Playing.into();
        self.current_frame = 0;
        self.audio_stream = None;
        self.children = BTreeMap::new();
    }

    fn id(&self) -> CharacterId {
        self.static_data.id
    }

    fn current_frame(&self) -> FrameNumber {
        self.current_frame
    }

    fn total_frames(&self) -> FrameNumber {
        self.static_data.total_frames
    }

    fn playing(&self) -> bool {
        self.flags.contains(MovieClipFlags::Playing)
    }

    fn set_playing(&mut self, value: bool) {
        if value {
            self.flags.insert(MovieClipFlags::Playing);
        } else {
            self.flags.remove(MovieClipFlags::Playing);
        }
    }

    fn first_child(&self) -> Option<DisplayObject<'gc>> {
        self.base.first_child()
    }
    fn set_first_child(
        &mut self,
        context: gc_arena::MutationContext<'gc, '_>,
        node: Option<DisplayObject<'gc>>,
    ) {
        self.base.set_first_child(context, node);
    }

    fn play(&mut self) {
        // Can only play clips with multiple frames.
        if self.total_frames() > 1 {
            self.set_playing(true);
        }
    }

    fn stop(&mut self, context: &mut UpdateContext<'_, 'gc, '_>) {
        self.set_playing(false);
        self.stop_audio_stream(context);
    }

    fn tag_stream_len(&self) -> usize {
        self.static_data.swf.end - self.static_data.swf.start
    }

    /// Adds a child to the front of the execution list.
    /// This does not affect the render list.
    fn add_child_to_exec_list(
        &mut self,
        gc_context: MutationContext<'gc, '_>,
        child: DisplayObject<'gc>,
    ) {
        if let Some(head) = self.first_child() {
            head.set_prev_sibling(gc_context, Some(child));
            child.set_next_sibling(gc_context, Some(head));
        }
        self.set_first_child(gc_context, Some(child));
    }
    /// Removes a child from the execution list.
    /// This does not affect the render list.
    fn remove_child_from_exec_list(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        mut child: DisplayObject<'gc>,
    ) {
        // Remove from children linked list.
        let prev = child.prev_sibling();
        let next = child.next_sibling();
        if let Some(prev) = prev {
            prev.set_next_sibling(context.gc_context, next);
        }
        if let Some(next) = next {
            next.set_prev_sibling(context.gc_context, prev);
        }
        if let Some(head) = self.first_child() {
            if DisplayObject::ptr_eq(head, child) {
                self.set_first_child(context.gc_context, next);
            }
        }
        // Flag child as removed.
        child.unload(context);
    }

    /// Handles a PlaceObject tag when running a goto action.
    #[inline]
    fn goto_place_object<'a>(
        &mut self,
        reader: &mut SwfStream<&'a [u8]>,
        tag_len: usize,
        version: u8,
        goto_commands: &mut Vec<GotoPlaceObject>,
        is_rewind: bool,
        index: usize,
    ) -> DecodeResult {
        let place_object = if version == 1 {
            reader.read_place_object(tag_len)
        } else {
            reader.read_place_object_2_or_3(version)
        }?;

        // We merge the deltas from this PlaceObject with the previous command.
        let depth = Depth::from(place_object.depth);
        let mut goto_place =
            GotoPlaceObject::new(self.current_frame(), place_object, is_rewind, index);
        if let Some(i) = goto_commands.iter().position(|o| o.depth() == depth) {
            goto_commands[i].merge(&mut goto_place);
        } else {
            goto_commands.push(goto_place);
        }

        Ok(())
    }

    /// Handle a RemoveObject tag when running a goto action.
    #[inline]
    fn goto_remove_object<'a>(
        &mut self,
        reader: &mut SwfStream<&'a [u8]>,
        version: u8,
        context: &mut UpdateContext<'_, 'gc, '_>,
        goto_commands: &mut Vec<GotoPlaceObject>,
        is_rewind: bool,
    ) -> DecodeResult {
        let remove_object = if version == 1 {
            reader.read_remove_object_1()
        } else {
            reader.read_remove_object_2()
        }?;
        let depth = Depth::from(remove_object.depth);
        if let Some(i) = goto_commands.iter().position(|o| o.depth() == depth) {
            goto_commands.swap_remove(i);
        }
        if !is_rewind {
            // For fast-forwards, if this tag were to remove an object
            // that existed before the goto, then we can remove that child right away.
            // Don't do this for rewinds, because they conceptually
            // start from an empty display list, and we also want to examine
            // the old children to decide if they persist (place_frame <= goto_frame).
            let child = self.children.remove(&depth);
            if let Some(child) = child {
                self.remove_child_from_exec_list(context, child);
            }
        }
        Ok(())
    }

    /// Run all actions for the given clip event.
    fn run_clip_event(
        &self,
        self_display_object: DisplayObject<'gc>,
        context: &mut UpdateContext<'_, 'gc, '_>,
        event: ClipEvent,
    ) -> ClipEventResult {
        let mut handled = ClipEventResult::NotHandled;

        // TODO: What's the behavior for loaded SWF files?
        if context.swf.version() >= 5 {
            for clip_action in self
                .clip_actions
                .iter()
                .filter(|action| action.event == event)
            {
                // KeyPress events are consumed by a single instance.
                if matches!(clip_action.event, ClipEvent::KeyPress { .. }) {
                    handled = ClipEventResult::Handled;
                }
                context.action_queue.queue_actions(
                    self_display_object,
                    ActionType::Normal {
                        bytecode: clip_action.action_data.clone(),
                    },
                    event == ClipEvent::Unload,
                );
            }

            // Queue ActionScript-defined event handlers after the SWF defined ones.
            // (e.g., clip.onEnterFrame = foo).
            if context.swf.version() >= 6 {
                if let Some(name) = event.method_name() {
                    context.action_queue.queue_actions(
                        self_display_object,
                        ActionType::Method {
                            object: self.object.unwrap(),
                            name,
                            args: vec![],
                        },
                        event == ClipEvent::Unload,
                    );
                }
            }
        }

        handled
    }

    /// Run clip actions that trigger after the clip's own actions.
    ///
    /// Currently, this is purely limited to `MovieClipLoader`'s `onLoadInit`
    /// event, delivered via the `LoadManager`. We need to be called here so
    /// that external init code runs after the event.
    ///
    /// TODO: If it turns out other `Load` events need to be delayed, perhaps
    /// we should change which frame triggers a `Load` event, rather than
    /// making sure our actions run after the clip's.
    fn run_clip_postevent(
        &self,
        self_display_object: DisplayObject<'gc>,
        context: &mut UpdateContext<'_, 'gc, '_>,
        event: ClipEvent,
    ) {
        // Finally, queue any loaders that may be waiting for this event.
        if let ClipEvent::Load = event {
            context.load_manager.movie_clip_on_load(
                self_display_object,
                self.object,
                context.action_queue,
                context.gc_context,
            );
        }
    }

    pub fn clip_actions(&self) -> &[ClipAction] {
        &self.clip_actions
    }

    pub fn set_clip_actions(&mut self, actions: Vec<ClipAction>) {
        self.clip_actions = actions;
    }

    fn initialized(&self) -> bool {
        self.flags.contains(MovieClipFlags::Initialized)
    }

    fn set_initialized(&mut self, value: bool) -> bool {
        if value {
            self.flags.insert(MovieClipFlags::Initialized)
        } else {
            self.flags.remove(MovieClipFlags::Initialized)
        }
    }

    /// Stops the audio stream if one is playing.
    fn stop_audio_stream(&mut self, context: &mut UpdateContext<'_, 'gc, '_>) {
        if let Some(audio_stream) = self.audio_stream.take() {
            context.audio.stop_stream(audio_stream);
        }
    }

    pub fn movie(&self) -> Arc<SwfMovie> {
        self.static_data.swf.movie.clone()
    }
}

// Preloading of definition tags
impl<'gc, 'a> MovieClipData<'gc> {
    #[inline]
    fn define_bits_lossless(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        version: u8,
    ) -> DecodeResult {
        let define_bits_lossless = reader.read_define_bits_lossless(version)?;
        let bitmap_info = context
            .renderer
            .register_bitmap_png(&define_bits_lossless)?;
        let bitmap = crate::display_object::Bitmap::new(
            context,
            define_bits_lossless.id,
            bitmap_info.handle,
            bitmap_info.width,
            bitmap_info.height,
        );
        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(define_bits_lossless.id, Character::Bitmap(bitmap));
        Ok(())
    }

    #[inline]
    fn define_morph_shape(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        morph_shapes: &mut fnv::FnvHashMap<CharacterId, MorphShapeStatic>,
        version: u8,
    ) -> DecodeResult {
        // Certain backends may have to preload morph shape frames, so defer registering until the end.
        let swf_shape = reader.read_define_morph_shape(version)?;
        let morph_shape = MorphShapeStatic::from_swf_tag(context.renderer, &swf_shape);
        morph_shapes.insert(swf_shape.id, morph_shape);
        Ok(())
    }

    #[inline]
    fn define_shape(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        version: u8,
    ) -> DecodeResult {
        let swf_shape = reader.read_define_shape(version)?;
        let graphic = Graphic::from_swf_tag(context, &swf_shape);
        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(swf_shape.id, Character::Graphic(graphic));
        Ok(())
    }

    #[inline]
    fn preload_place_object(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        tag_len: usize,
        ids: &mut fnv::FnvHashMap<Depth, CharacterId>,
        morph_shapes: &mut fnv::FnvHashMap<CharacterId, MorphShapeStatic>,
        version: u8,
    ) -> DecodeResult {
        use swf::PlaceObjectAction;
        let place_object = if version == 1 {
            reader.read_place_object(tag_len)
        } else {
            reader.read_place_object_2_or_3(version)
        }?;
        match place_object.action {
            PlaceObjectAction::Place(id) => {
                if let Some(morph_shape) = morph_shapes.get_mut(&id) {
                    ids.insert(place_object.depth.into(), id);
                    if let Some(ratio) = place_object.ratio {
                        morph_shape.register_ratio(context.renderer, ratio);
                    }
                }
            }
            PlaceObjectAction::Modify => {
                if let Some(&id) = ids.get(&place_object.depth.into()) {
                    if let Some(morph_shape) = morph_shapes.get_mut(&id) {
                        ids.insert(place_object.depth.into(), id);
                        if let Some(ratio) = place_object.ratio {
                            morph_shape.register_ratio(context.renderer, ratio);
                        }
                    }
                }
            }
            PlaceObjectAction::Replace(id) => {
                if let Some(morph_shape) = morph_shapes.get_mut(&id) {
                    ids.insert(place_object.depth.into(), id);
                    if let Some(ratio) = place_object.ratio {
                        morph_shape.register_ratio(context.renderer, ratio);
                    }
                } else {
                    ids.remove(&place_object.depth.into());
                }
            }
        };

        Ok(())
    }

    #[inline]
    fn preload_sound_stream_block(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        cur_frame: FrameNumber,
        static_data: &mut MovieClipStatic,
        tag_len: usize,
    ) -> DecodeResult {
        if static_data.audio_stream_info.is_some() {
            let pos = reader.get_ref().position() as usize;
            let data = reader.get_ref().get_ref();
            let data = &data[pos..pos + tag_len];
            context
                .audio
                .preload_sound_stream_block(self.id(), cur_frame, data);
        }

        Ok(())
    }

    #[inline]
    fn preload_sound_stream_head(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        cur_frame: FrameNumber,
        static_data: &mut MovieClipStatic,
        _version: u8,
    ) -> DecodeResult {
        let audio_stream_info = reader.read_sound_stream_head()?;
        context
            .audio
            .preload_sound_stream_head(self.id(), cur_frame, &audio_stream_info);
        static_data.audio_stream_info = Some(audio_stream_info);
        Ok(())
    }

    #[inline]
    fn define_bits(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        tag_len: usize,
    ) -> DecodeResult {
        use std::io::Read;
        let id = reader.read_u16()?;
        let data_len = tag_len - 2;
        let mut jpeg_data = Vec::with_capacity(data_len);
        reader.get_mut().read_to_end(&mut jpeg_data)?;
        let bitmap_info = context.renderer.register_bitmap_jpeg(
            id,
            &jpeg_data,
            context
                .library
                .library_for_movie_mut(self.movie())
                .jpeg_tables(),
        )?;
        let bitmap = crate::display_object::Bitmap::new(
            context,
            id,
            bitmap_info.handle,
            bitmap_info.width,
            bitmap_info.height,
        );
        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(id, Character::Bitmap(bitmap));
        Ok(())
    }

    #[inline]
    fn define_bits_jpeg_2(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        tag_len: usize,
    ) -> DecodeResult {
        use std::io::Read;
        let id = reader.read_u16()?;
        let data_len = tag_len - 2;
        let mut jpeg_data = Vec::with_capacity(data_len);
        reader.get_mut().read_to_end(&mut jpeg_data)?;
        let bitmap_info = context.renderer.register_bitmap_jpeg_2(id, &jpeg_data)?;
        let bitmap = crate::display_object::Bitmap::new(
            context,
            id,
            bitmap_info.handle,
            bitmap_info.width,
            bitmap_info.height,
        );
        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(id, Character::Bitmap(bitmap));
        Ok(())
    }

    #[inline]
    fn define_bits_jpeg_3(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        tag_len: usize,
    ) -> DecodeResult {
        use std::io::Read;
        let id = reader.read_u16()?;
        let jpeg_len = reader.read_u32()? as usize;
        let alpha_len = tag_len - 6 - jpeg_len;
        let mut jpeg_data = Vec::with_capacity(jpeg_len);
        let mut alpha_data = Vec::with_capacity(alpha_len);
        reader
            .get_mut()
            .take(jpeg_len as u64)
            .read_to_end(&mut jpeg_data)?;
        reader
            .get_mut()
            .take(alpha_len as u64)
            .read_to_end(&mut alpha_data)?;
        let bitmap_info = context
            .renderer
            .register_bitmap_jpeg_3(id, &jpeg_data, &alpha_data)?;
        let bitmap = Bitmap::new(
            context,
            id,
            bitmap_info.handle,
            bitmap_info.width,
            bitmap_info.height,
        );
        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(id, Character::Bitmap(bitmap));
        Ok(())
    }

    #[inline]
    fn define_bits_jpeg_4(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        tag_len: usize,
    ) -> DecodeResult {
        use std::io::Read;
        let id = reader.read_u16()?;
        let jpeg_len = reader.read_u32()? as usize;
        let _deblocking = reader.read_u16()?;
        let alpha_len = tag_len - 6 - jpeg_len;
        let mut jpeg_data = Vec::with_capacity(jpeg_len);
        let mut alpha_data = Vec::with_capacity(alpha_len);
        reader
            .get_mut()
            .take(jpeg_len as u64)
            .read_to_end(&mut jpeg_data)?;
        reader
            .get_mut()
            .take(alpha_len as u64)
            .read_to_end(&mut alpha_data)?;
        let bitmap_info = context
            .renderer
            .register_bitmap_jpeg_3(id, &jpeg_data, &alpha_data)?;
        let bitmap = Bitmap::new(
            context,
            id,
            bitmap_info.handle,
            bitmap_info.width,
            bitmap_info.height,
        );
        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(id, Character::Bitmap(bitmap));
        Ok(())
    }

    #[inline]
    fn define_button_1(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
    ) -> DecodeResult {
        let swf_button = reader.read_define_button_1()?;
        let button = Button::from_swf_tag(
            &swf_button,
            &self.static_data.swf,
            &context.library,
            context.gc_context,
        );
        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(swf_button.id, Character::Button(button));
        Ok(())
    }

    #[inline]
    fn define_button_2(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
    ) -> DecodeResult {
        let swf_button = reader.read_define_button_2()?;
        let button = Button::from_swf_tag(
            &swf_button,
            &self.static_data.swf,
            &context.library,
            context.gc_context,
        );
        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(swf_button.id, Character::Button(button));
        Ok(())
    }

    #[inline]
    fn define_button_cxform(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        tag_len: usize,
    ) -> DecodeResult {
        let button_colors = reader.read_define_button_cxform(tag_len)?;
        if let Some(button) = context
            .library
            .library_for_movie_mut(self.movie())
            .get_character_by_id(button_colors.id)
        {
            if let Character::Button(button) = button {
                button.set_colors(context.gc_context, &button_colors.color_transforms[..]);
            } else {
                log::warn!(
                    "DefineButtonCxform: Tried to apply on non-button ID {}",
                    button_colors.id
                );
            }
        } else {
            log::warn!(
                "DefineButtonCxform: Character ID {} doesn't exist",
                button_colors.id
            );
        }
        Ok(())
    }

    #[inline]
    fn define_button_sound(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
    ) -> DecodeResult {
        let button_sounds = reader.read_define_button_sound()?;
        if let Some(button) = context
            .library
            .library_for_movie_mut(self.movie())
            .get_character_by_id(button_sounds.id)
        {
            if let Character::Button(button) = button {
                button.set_sounds(context.gc_context, button_sounds);
            } else {
                log::warn!(
                    "DefineButtonSound: Tried to apply on non-button ID {}",
                    button_sounds.id
                );
            }
        } else {
            log::warn!(
                "DefineButtonSound: Character ID {} doesn't exist",
                button_sounds.id
            );
        }
        Ok(())
    }

    /// Defines a dynamic text field character.
    #[inline]
    fn define_edit_text(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
    ) -> DecodeResult {
        let swf_edit_text = reader.read_define_edit_text()?;
        let edit_text = EditText::from_swf_tag(context, self.movie(), swf_edit_text);
        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(edit_text.id(), Character::EditText(edit_text));
        Ok(())
    }

    #[inline]
    fn define_font_1(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
    ) -> DecodeResult {
        let font = reader.read_define_font_1()?;
        let glyphs = font
            .glyphs
            .into_iter()
            .map(|g| swf::Glyph {
                shape_records: g,
                code: 0,
                advance: None,
                bounds: None,
            })
            .collect::<Vec<_>>();

        let font = swf::Font {
            id: font.id,
            version: 0,
            name: "".to_string(),
            glyphs,
            language: swf::Language::Unknown,
            layout: None,
            is_small_text: false,
            is_shift_jis: false,
            is_ansi: false,
            is_bold: false,
            is_italic: false,
        };
        let font_object = Font::from_swf_tag(context.gc_context, context.renderer, &font).unwrap();
        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(font.id, Character::Font(font_object));
        Ok(())
    }

    #[inline]
    fn define_font_2(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
    ) -> DecodeResult {
        let font = reader.read_define_font_2(2)?;
        let font_object = Font::from_swf_tag(context.gc_context, context.renderer, &font).unwrap();
        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(font.id, Character::Font(font_object));
        Ok(())
    }

    #[inline]
    fn define_font_3(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
    ) -> DecodeResult {
        let font = reader.read_define_font_2(3)?;
        let font_object = Font::from_swf_tag(context.gc_context, context.renderer, &font).unwrap();
        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(font.id, Character::Font(font_object));

        Ok(())
    }

    #[inline]
    fn define_sound(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
    ) -> DecodeResult {
        let sound = reader.read_define_sound()?;
        if let Ok(handle) = context.audio.register_sound(&sound) {
            context
                .library
                .library_for_movie_mut(self.movie())
                .register_character(sound.id, Character::Sound(handle));
        } else {
            log::error!(
                "MovieClip::define_sound: Unable to register sound ID {}",
                sound.id
            );
        }
        Ok(())
    }

    fn define_sprite(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        tag_len: usize,
        morph_shapes: &mut fnv::FnvHashMap<CharacterId, MorphShapeStatic>,
    ) -> DecodeResult {
        let id = reader.read_character_id()?;
        let num_frames = reader.read_u16()?;
        let movie_clip = MovieClip::new_with_data(
            context.gc_context,
            id,
            self.static_data
                .swf
                .resize_to_reader(reader, tag_len - 4)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "Cannot define sprite with invalid offset and length!",
                    )
                })?,
            num_frames,
        );

        movie_clip.preload(context, morph_shapes);

        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(id, Character::MovieClip(movie_clip));

        Ok(())
    }

    #[inline]
    fn define_text(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        version: u8,
    ) -> DecodeResult {
        let text = reader.read_define_text(version)?;
        let text_object = Text::from_swf_tag(context, self.movie(), &text);
        context
            .library
            .library_for_movie_mut(self.movie())
            .register_character(text.id, Character::Text(text_object));
        Ok(())
    }

    #[inline]
    fn script_limits(
        &mut self,
        reader: &mut SwfStream<&'a [u8]>,
        avm: &mut Avm1<'gc>,
    ) -> DecodeResult {
        let max_recursion_depth = reader.read_u16()?;
        let _timeout_in_seconds = reader.read_u16()?;

        avm.set_max_recursion_depth(max_recursion_depth);

        Ok(())
    }

    #[inline]
    fn export_assets(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
    ) -> DecodeResult {
        let exports = reader.read_export_assets()?;
        for export in exports {
            context
                .library
                .library_for_movie_mut(self.movie())
                .register_export(export.id, &export.name);
        }
        Ok(())
    }

    #[inline]
    fn frame_label(
        &mut self,
        _context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        tag_len: usize,
        cur_frame: FrameNumber,
        static_data: &mut MovieClipStatic,
    ) -> DecodeResult {
        let mut frame_label = reader.read_frame_label(tag_len)?;
        // Frame labels are case insensitive (ASCII).
        frame_label.label.make_ascii_lowercase();
        if let std::collections::hash_map::Entry::Vacant(v) =
            static_data.frame_labels.entry(frame_label.label)
        {
            v.insert(cur_frame);
        } else {
            log::warn!("Movie clip {}: Duplicated frame label", self.id());
        }
        Ok(())
    }

    #[inline]
    fn jpeg_tables(
        &mut self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        tag_len: usize,
    ) -> DecodeResult {
        use std::io::Read;
        // TODO(Herschel): Can we use a slice instead of copying?
        let mut jpeg_data = Vec::with_capacity(tag_len);
        reader.get_mut().read_to_end(&mut jpeg_data)?;
        context
            .library
            .library_for_movie_mut(self.movie())
            .set_jpeg_tables(jpeg_data);
        Ok(())
    }

    #[inline]
    fn preload_remove_object(
        &mut self,
        _context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        ids: &mut fnv::FnvHashMap<Depth, CharacterId>,
        version: u8,
    ) -> DecodeResult {
        let remove_object = if version == 1 {
            reader.read_remove_object_1()
        } else {
            reader.read_remove_object_2()
        }?;
        ids.remove(&remove_object.depth.into());
        Ok(())
    }

    #[inline]
    fn preload_show_frame(
        &mut self,
        _context: &mut UpdateContext<'_, 'gc, '_>,
        _reader: &mut SwfStream<&'a [u8]>,
        cur_frame: &mut FrameNumber,
    ) -> DecodeResult {
        *cur_frame += 1;
        Ok(())
    }
}

// Control tags
impl<'gc, 'a> MovieClip<'gc> {
    #[inline]
    fn do_action(
        self,
        self_display_object: DisplayObject<'gc>,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        tag_len: usize,
    ) -> DecodeResult {
        // Queue the actions.
        let slice = self
            .0
            .read()
            .static_data
            .swf
            .resize_to_reader(reader, tag_len)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Invalid source or tag length when running action",
                )
            })?;
        context.action_queue.queue_actions(
            self_display_object,
            ActionType::Normal { bytecode: slice },
            false,
        );
        Ok(())
    }

    fn place_object(
        self,
        self_display_object: DisplayObject<'gc>,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        tag_len: usize,
        version: u8,
    ) -> DecodeResult {
        let place_object = if version == 1 {
            reader.read_place_object(tag_len)
        } else {
            reader.read_place_object_2_or_3(version)
        }?;
        use swf::PlaceObjectAction;
        match place_object.action {
            PlaceObjectAction::Place(id) | PlaceObjectAction::Replace(id) => {
                if let Some(child) = self.instantiate_child(
                    self_display_object,
                    context,
                    id,
                    place_object.depth.into(),
                    &place_object,
                    matches!(place_object.action, PlaceObjectAction::Replace(_)),
                ) {
                    child
                } else {
                    return Ok(());
                }
            }
            PlaceObjectAction::Modify => {
                if let Some(mut child) = self
                    .0
                    .read()
                    .children
                    .get(&place_object.depth.into())
                    .copied()
                {
                    child.apply_place_object(context.gc_context, &place_object);
                    child
                } else {
                    return Ok(());
                }
            }
        };

        Ok(())
    }

    #[inline]
    fn remove_object(
        self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
        version: u8,
    ) -> DecodeResult {
        let remove_object = if version == 1 {
            reader.read_remove_object_1()
        } else {
            reader.read_remove_object_2()
        }?;
        let mut mc = self.0.write(context.gc_context);
        let child = mc.children.remove(&remove_object.depth.into());
        if let Some(child) = child {
            mc.remove_child_from_exec_list(context, child);
        }
        Ok(())
    }

    #[inline]
    fn set_background_color(
        self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
    ) -> DecodeResult {
        *context.background_color = reader.read_rgb()?;
        Ok(())
    }

    #[inline]
    fn sound_stream_block(
        self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        _reader: &mut SwfStream<&'a [u8]>,
    ) -> DecodeResult {
        let mut mc = self.0.write(context.gc_context);
        if let (Some(stream_info), None) = (&mc.static_data.audio_stream_info, mc.audio_stream) {
            let slice = mc
                .static_data
                .swf
                .to_start_and_end(mc.tag_stream_pos as usize, mc.tag_stream_len())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "Invalid slice generated when constructing sound stream block",
                    )
                })?;
            let audio_stream =
                context
                    .audio
                    .start_stream(mc.id(), mc.current_frame() + 1, slice, &stream_info);
            mc.audio_stream = audio_stream.ok();
        }

        Ok(())
    }

    #[inline]
    fn start_sound_1(
        self,
        context: &mut UpdateContext<'_, 'gc, '_>,
        reader: &mut SwfStream<&'a [u8]>,
    ) -> DecodeResult {
        let start_sound = reader.read_start_sound_1()?;
        if let Some(handle) = context
            .library
            .library_for_movie_mut(self.movie().unwrap()) // TODO
            .get_sound(start_sound.id)
        {
            use swf::SoundEvent;
            // The sound event type is controlled by the "Sync" setting in the Flash IDE.
            match start_sound.sound_info.event {
                // "Event" sounds always play, independent of the timeline.
                SoundEvent::Event => {
                    let _ = context.audio.start_sound(handle, &start_sound.sound_info);
                }

                // "Start" sounds only play if an instance of the same sound is not already playing.
                SoundEvent::Start => {
                    if !context.audio.is_sound_playing_with_handle(handle) {
                        let _ = context.audio.start_sound(handle, &start_sound.sound_info);
                    }
                }

                // "Stop" stops any active instances of a given sound.
                SoundEvent::Stop => context.audio.stop_sounds_with_handle(handle),
            }
        }
        Ok(())
    }
}

/// Static data shared between all instances of a movie clip.
#[allow(dead_code)]
#[derive(Clone)]
struct MovieClipStatic {
    id: CharacterId,
    swf: SwfSlice,
    frame_labels: HashMap<String, FrameNumber>,
    audio_stream_info: Option<swf::SoundStreamHead>,
    total_frames: FrameNumber,
}

impl MovieClipStatic {
    fn empty(swf: SwfSlice) -> Self {
        Self {
            id: 0,
            swf,
            total_frames: 1,
            frame_labels: HashMap::new(),
            audio_stream_info: None,
        }
    }
}

unsafe impl<'gc> Collect for MovieClipStatic {
    #[inline]
    fn needs_trace() -> bool {
        false
    }
}

/// Stores the placement settings for display objects during a
/// goto command.
#[derive(Debug)]
struct GotoPlaceObject {
    /// The frame number that this character was first placed on.
    frame: FrameNumber,
    /// The display properties of the object.
    place_object: swf::PlaceObject,
    /// Increasing index of this place command, for sorting.
    index: usize,
}

impl GotoPlaceObject {
    fn new(
        frame: FrameNumber,
        mut place_object: swf::PlaceObject,
        is_rewind: bool,
        index: usize,
    ) -> Self {
        if is_rewind {
            if let swf::PlaceObjectAction::Place(_) = place_object.action {
                if place_object.matrix.is_none() {
                    place_object.matrix = Some(Default::default());
                }
                if place_object.color_transform.is_none() {
                    place_object.color_transform = Some(Default::default());
                }
                if place_object.ratio.is_none() {
                    place_object.ratio = Some(Default::default());
                }
                if place_object.name.is_none() {
                    place_object.name = Some(Default::default());
                }
                if place_object.clip_depth.is_none() {
                    place_object.clip_depth = Some(Default::default());
                }
                if place_object.class_name.is_none() {
                    place_object.class_name = Some(Default::default());
                }
            }
        }

        Self {
            frame,
            place_object,
            index,
        }
    }

    #[inline]
    fn id(&self) -> CharacterId {
        match &self.place_object.action {
            swf::PlaceObjectAction::Place(id) | swf::PlaceObjectAction::Replace(id) => *id,
            swf::PlaceObjectAction::Modify => 0,
        }
    }

    #[inline]
    fn modifies_original_item(&self) -> bool {
        matches!(
            &self.place_object.action,
            swf::PlaceObjectAction::Replace(_)
        )
    }

    #[inline]
    fn depth(&self) -> Depth {
        self.place_object.depth.into()
    }

    fn merge(&mut self, next: &mut GotoPlaceObject) {
        use swf::PlaceObjectAction;
        let cur_place = &mut self.place_object;
        let next_place = &mut next.place_object;
        match (cur_place.action, next_place.action) {
            (cur, PlaceObjectAction::Modify) => {
                cur_place.action = cur;
            }
            (_, new) => {
                cur_place.action = new;
                self.frame = next.frame;
            }
        };
        if next_place.matrix.is_some() {
            cur_place.matrix = next_place.matrix.take();
        }
        if next_place.color_transform.is_some() {
            cur_place.color_transform = next_place.color_transform.take();
        }
        if next_place.ratio.is_some() {
            cur_place.ratio = next_place.ratio.take();
        }
        if next_place.name.is_some() {
            cur_place.name = next_place.name.take();
        }
        if next_place.clip_depth.is_some() {
            cur_place.clip_depth = next_place.clip_depth.take();
        }
        if next_place.class_name.is_some() {
            cur_place.class_name = next_place.class_name.take();
        }
        if next_place.background_color.is_some() {
            cur_place.background_color = next_place.background_color.take();
        }
        // TODO: Other stuff.
    }
}

/// Boolean state flags used by `MovieClip`.
#[derive(Debug, EnumSetType)]
enum MovieClipFlags {
    /// Whether this `MovieClip` has run its initial frame.
    Initialized,

    /// Whether this `MovieClip` is playing or stopped.
    Playing,
}

/// Actions that are attached to a `MovieClip` event in
/// an `onClipEvent`/`on` handler.
#[derive(Debug, Clone)]
pub struct ClipAction {
    /// The event that triggers this handler.
    event: ClipEvent,

    /// The actions to run.
    action_data: SwfSlice,
}

impl ClipAction {
    /// Build a set of clip actions from a SWF movie and a parsed ClipAction.
    ///
    /// TODO: Our underlying SWF parser currently does not yield slices of the
    /// underlying movie, so we cannot convert those slices into a `SwfSlice`.
    /// Instead, we have to construct a fake `SwfMovie` just to hold one clip
    /// action.
    pub fn from_action_and_movie(
        other: swf::ClipAction,
        movie: Arc<SwfMovie>,
    ) -> impl Iterator<Item = Self> {
        use swf::ClipEventFlag;

        let len = other.action_data.len();
        let key_code = other.key_code;
        let movie = Arc::new(movie.from_movie_and_subdata(other.action_data, &movie));
        other.events.into_iter().map(move |event| Self {
            event: match event {
                ClipEventFlag::Construct => ClipEvent::Construct,
                ClipEventFlag::Data => ClipEvent::Data,
                ClipEventFlag::DragOut => ClipEvent::DragOut,
                ClipEventFlag::DragOver => ClipEvent::DragOver,
                ClipEventFlag::EnterFrame => ClipEvent::EnterFrame,
                ClipEventFlag::Initialize => ClipEvent::Initialize,
                ClipEventFlag::KeyUp => ClipEvent::KeyUp,
                ClipEventFlag::KeyDown => ClipEvent::KeyDown,
                ClipEventFlag::KeyPress => ClipEvent::KeyPress {
                    key_code: key_code
                        .and_then(|k| ButtonKeyCode::try_from(k).ok())
                        .unwrap_or(ButtonKeyCode::Unknown),
                },
                ClipEventFlag::Load => ClipEvent::Load,
                ClipEventFlag::MouseUp => ClipEvent::MouseUp,
                ClipEventFlag::MouseDown => ClipEvent::MouseDown,
                ClipEventFlag::MouseMove => ClipEvent::MouseMove,
                ClipEventFlag::Press => ClipEvent::Press,
                ClipEventFlag::RollOut => ClipEvent::RollOut,
                ClipEventFlag::RollOver => ClipEvent::RollOver,
                ClipEventFlag::Release => ClipEvent::Release,
                ClipEventFlag::ReleaseOutside => ClipEvent::ReleaseOutside,
                ClipEventFlag::Unload => ClipEvent::Unload,
            },
            action_data: SwfSlice {
                movie: Arc::clone(&movie),
                start: 0,
                end: len,
            },
        })
    }
}
