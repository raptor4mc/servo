/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::ops::Range;
use std::sync::Arc;

use fonts::{ShapedText, ShapedTextSlice, ShapedTextSlicer, ShapingOptions};
use icu_segmenter::LineBreakOptions;
use style::computed_values::white_space_collapse::T as WhiteSpaceCollapse;
use style::computed_values::word_break::T as WordBreak;
use style::properties::ComputedValues;
use style::str::char_is_whitespace;
use style::values::computed::OverflowWrap;
use unicode_script::Script;

use crate::ArcRefCell;
use crate::flow::inline::line_breaker::LineBreaker;
use crate::flow::inline::text_run::{FontAndScriptInfo, TextRun, TextRunItem, script_is_specific};

pub(crate) struct ShapingQueueText {
    pub info: Arc<FontAndScriptInfo>,
    pub byte_range: Range<usize>,
    pub character_range: Range<usize>,
    pub text_run: ArcRefCell<TextRun>,
    pub index_in_text_run: usize,
    pub old_shaped_text: Option<Arc<ShapedText>>,
}

impl ShapingQueueText {
    pub(crate) fn slice_shaped_text(
        &self,
        shaped_text_slicer: &mut ShapedTextSlicer,
        batch_start_character_offset: usize,
        parent_style: &ComputedValues,
        formatting_context_text: &str,
        line_breaker: &mut LineBreaker,
    ) -> (Vec<Arc<ShapedTextSlice>>, bool) {
        // Gather the linebreaks that apply to this segment from the inline formatting context's collection
        // of line breaks. Also add a simulated break at the end of the segment in order to ensure the final
        // piece of text is processed.
        let range = self.byte_range.clone();
        let linebreaks = line_breaker.advance_to_linebreaks_in_range(self.byte_range.clone());
        let linebreak_iter = linebreaks.iter().chain(std::iter::once(&range.end));

        let mut runs = Vec::with_capacity(linebreaks.len());
        let mut break_at_start = false;

        let text_style = parent_style.get_inherited_text();
        let can_break_anywhere = text_style.word_break == WordBreak::BreakAll ||
            text_style.overflow_wrap == OverflowWrap::Anywhere ||
            text_style.overflow_wrap == OverflowWrap::BreakWord;

        let mut last_slice = self.byte_range.start..self.byte_range.start;
        let mut current_character_offset =
            self.character_range.start - batch_start_character_offset;
        for break_index in linebreak_iter {
            if *break_index == self.byte_range.start {
                break_at_start = true;
                continue;
            }

            // Extend the slice to the next UAX#14 line break opportunity.
            let mut slice = last_slice.end..*break_index;
            let word = &formatting_context_text[slice.clone()];

            // Split off any trailing whitespace into a separate glyph run.
            let mut whitespace = slice.end..slice.end;
            let rev_char_indices = word.char_indices().rev().peekable();

            let mut non_whitespace_slice_ends_with_whitespace = false;
            let mut ends_with_whitespace = false;
            if let Some((first_white_space_index, first_white_space_character)) = rev_char_indices
                .take_while(|&(_, character)| char_is_whitespace(character))
                .last()
            {
                ends_with_whitespace = true;
                whitespace.start = slice.start + first_white_space_index;

                // If line breaking for a piece of text that has `white-space-collapse:
                // break-spaces` there is a line break opportunity *after* every preserved space,
                // but not before. This means that we should not split off the first whitespace.
                //
                // An exception to this is if the style tells us that we can break in the middle of words.
                if text_style.white_space_collapse == WhiteSpaceCollapse::BreakSpaces &&
                    !can_break_anywhere
                {
                    whitespace.start += first_white_space_character.len_utf8();
                    non_whitespace_slice_ends_with_whitespace = true;
                }

                slice.end = whitespace.start;
            }

            // If there's no whitespace and `word-break` is set to `keep-all`, try increasing the slice.
            // TODO: This should only happen for CJK text.
            if !ends_with_whitespace &&
                *break_index != self.byte_range.end &&
                text_style.word_break == WordBreak::KeepAll &&
                !can_break_anywhere
            {
                continue;
            }

            // Only advance the last slice if we are not going to try to expand the slice.
            last_slice = slice.start..*break_index;

            // Push the non-whitespace part of the range.
            if !slice.is_empty() {
                current_character_offset += formatting_context_text[slice].chars().count();
                if let Some(slice) = shaped_text_slicer.slice_until_character_offset(
                    current_character_offset,
                    false, /* is_whitespace */
                    non_whitespace_slice_ends_with_whitespace,
                ) {
                    runs.push(slice);
                }
            }

            if whitespace.is_empty() {
                continue;
            }

            // If `white-space-collapse: break-spaces` is active, insert a line breaking opportunity
            // between each white space character in the white space that we trimmed off.
            if text_style.white_space_collapse == WhiteSpaceCollapse::BreakSpaces {
                for _ in formatting_context_text[whitespace].chars() {
                    current_character_offset += 1;
                    if let Some(slice) = shaped_text_slicer.slice_until_character_offset(
                        current_character_offset,
                        true, /* is_whitespace */
                        true, /* ends_with_whitespace */
                    ) {
                        runs.push(slice);
                    }
                }
                continue;
            }

            current_character_offset += formatting_context_text[whitespace].chars().count();
            if let Some(slice) = shaped_text_slicer.slice_until_character_offset(
                current_character_offset,
                true, /* is_whitespace */
                true, /* ends_with_whitespace */
            ) {
                runs.push(slice);
            }
        }

        (runs, break_at_start)
    }
}

pub(crate) enum ShapingQueueEntry {
    Flush,
    Text(ShapingQueueText),
}

impl ShapingQueueEntry {
    pub(crate) fn new(
        text_run: ArcRefCell<TextRun>,
        text_run_item: &TextRunItem,
        index_in_text_run: usize,
        old_text_run_line_item: Option<TextRunItem>,
    ) -> Self {
        let TextRunItem::TextSegment(text_segment) = text_run_item else {
            return Self::Flush;
        };

        let old_shaped_text = old_text_run_line_item.and_then(|old_text_run_line_item| {
            let TextRunItem::TextSegment(old_text_segment) = old_text_run_line_item else {
                return None;
            };
            if !text_segment.is_compatible_with_old_shaping_result(&old_text_segment) {
                return None;
            }
            Some(old_text_segment.runs.first()?.shaped_text())
        });

        Self::Text(ShapingQueueText {
            info: text_segment.info.clone(),
            byte_range: text_segment.byte_range.clone(),
            character_range: text_segment.character_range.clone(),
            text_run,
            index_in_text_run,
            old_shaped_text,
        })
    }
}

pub struct ShapingQueue<'a> {
    text: &'a str,
    queue: Vec<ShapingQueueText>,
    line_breaker: LineBreaker,
    resolved_script: Option<Script>,
}

impl<'a> ShapingQueue<'a> {
    pub(crate) fn new(text: &'a str, line_break_options: LineBreakOptions) -> Self {
        Self {
            text,
            queue: Default::default(),
            line_breaker: LineBreaker::new(text, line_break_options),
            resolved_script: None,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub(crate) fn compatible_old_shaping_result(
        &self,
        character_count: usize,
    ) -> Option<Arc<ShapedText>> {
        let old_shaped_text = self
            .queue
            .iter()
            .find_map(|entry| entry.old_shaped_text.as_ref())?;
        if old_shaped_text.character_count() != character_count {
            return None;
        }

        if !self.queue.iter().all(|entry| {
            entry
                .old_shaped_text
                .as_ref()
                .is_some_and(|entry_old_shaped_text| {
                    Arc::ptr_eq(old_shaped_text, entry_old_shaped_text)
                })
        }) {
            return None;
        }
        Some(old_shaped_text.clone())
    }

    pub(crate) fn shape_batch(&self) -> Option<Arc<ShapedText>> {
        let first = self.queue.first()?;
        let last = self.queue.last()?;

        let character_count = last.character_range.end - first.character_range.start;
        if let Some(old_shaping_result) = self.compatible_old_shaping_result(character_count) {
            return Some(old_shaping_result);
        };

        let mut options: ShapingOptions = (&*first.info).into();
        options.script = self.resolved_script.unwrap_or(first.info.script);

        let font = &first.info.font;
        Some(font.shape_text(
            &self.text[first.byte_range.start..last.byte_range.end],
            &options,
        ))
    }

    pub(crate) fn flush_batch(&mut self) {
        // If no shaped text is returned that means the batch is empty.
        let Some(shaped_text) = self.shape_batch() else {
            return;
        };

        let mut slicer = ShapedTextSlicer::new(shaped_text);
        let mut batch_start_character_offset = None;
        for entry in self.queue.drain(..) {
            let mut text_run = entry.text_run.borrow_mut();
            let (runs, break_at_start) = {
                let style = text_run.inline_styles.style.borrow().clone();
                let batch_start_character_offset =
                    batch_start_character_offset.get_or_insert(entry.character_range.start);
                entry.slice_shaped_text(
                    &mut slicer,
                    *batch_start_character_offset,
                    &style,
                    self.text,
                    &mut self.line_breaker,
                )
            };

            if let TextRunItem::TextSegment(text_segment) =
                &mut text_run.items[entry.index_in_text_run]
            {
                text_segment.runs = runs;
                text_segment.break_at_start = break_at_start;
            }
        }

        self.queue.clear();
        self.resolved_script = None;
    }

    fn update_resolved_script(&mut self, script: Script) {
        if self.resolved_script.is_none() && script_is_specific(script) {
            self.resolved_script = Some(script);
        }
    }

    fn compatible_with_current_batch(&self, text: &ShapingQueueText) -> bool {
        // If the queue is empty, we can always add new text to the batch.
        let Some(last) = self.queue.last() else {
            return true;
        };

        // If the `FontAndScriptInfo` (apart from Script) is not compatible, this
        // new `ShapingQueueText` isn't compatible.
        if last.character_range.end != text.character_range.start ||
            !last.info.eq_ignoring_script(&text.info)
        {
            return false;
        }

        debug_assert!(last.byte_range.end == text.byte_range.start);

        // Any resolved `Script` has to be compatible with any new specific `Script`.
        !script_is_specific(text.info.script) ||
            self.resolved_script
                .is_none_or(|resolved_script| resolved_script == text.info.script)
    }

    fn push_text(&mut self, text: ShapingQueueText) {
        if !self.compatible_with_current_batch(&text) {
            self.flush_batch();
        }
        self.update_resolved_script(text.info.script);
        self.queue.push(text);
    }

    pub(crate) fn push(&mut self, entry: ShapingQueueEntry) {
        match entry {
            ShapingQueueEntry::Flush => self.flush_batch(),
            ShapingQueueEntry::Text(shaping_queue_text) => self.push_text(shaping_queue_text),
        }
    }
}
