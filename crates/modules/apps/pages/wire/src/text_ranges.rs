use super::{InlineMark, MAX_SPAN_MARKS_PER_BLOCK, PageError, RelativeAnchor, SpanMark};

#[derive(Clone, Copy)]
pub struct TextEdit {
    start: u32,
    old_end: u32,
    new_end: u32,
}

pub fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

fn offset_of(chars: &[char]) -> u32 {
    chars.iter().map(|c| c.len_utf16() as u32).sum()
}

pub fn edit_between(old: &str, new: &str) -> Option<TextEdit> {
    if old == new {
        return None;
    }
    let old: Vec<char> = old.chars().collect();
    let new: Vec<char> = new.chars().collect();
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let start = offset_of(&old[..prefix]);
    Some(TextEdit {
        start,
        old_end: start + offset_of(&old[prefix..old.len() - suffix]),
        new_end: start + offset_of(&new[prefix..new.len() - suffix]),
    })
}

fn map_start(pos: u32, edit: TextEdit) -> u32 {
    if pos < edit.start {
        pos
    } else if pos >= edit.old_end {
        pos.saturating_add(edit.new_end)
            .saturating_sub(edit.old_end)
    } else {
        edit.start
    }
}

fn map_end(pos: u32, edit: TextEdit) -> u32 {
    if pos <= edit.start {
        pos
    } else if pos >= edit.old_end {
        pos.saturating_add(edit.new_end)
            .saturating_sub(edit.old_end)
    } else {
        edit.new_end
    }
}

pub fn rebase_anchor(anchor: &mut RelativeAnchor, edit: TextEdit, new_len: u32) {
    if anchor.start == anchor.end {
        let pos = if anchor.start <= edit.start {
            anchor.start
        } else if anchor.start >= edit.old_end {
            anchor
                .start
                .saturating_add(edit.new_end)
                .saturating_sub(edit.old_end)
        } else {
            edit.new_end
        }
        .min(new_len);
        anchor.start = pos;
        anchor.end = pos;
        return;
    }
    anchor.start = map_start(anchor.start, edit).min(new_len);
    anchor.end = map_end(anchor.end, edit).min(new_len).max(anchor.start);
}

fn normalize(marks: &mut Vec<SpanMark>) {
    marks.sort_by_key(|mark| (mark.kind, mark.start, mark.end));
    let mut out: Vec<SpanMark> = Vec::with_capacity(marks.len());
    for mark in marks.drain(..) {
        if let Some(last) = out.last_mut()
            && last.kind == mark.kind
            && mark.start <= last.end
        {
            last.end = last.end.max(mark.end);
        } else {
            out.push(mark);
        }
    }
    *marks = out;
}

pub fn validate_marks(
    text: &str,
    mut marks: Vec<SpanMark>,
) -> Result<Vec<SpanMark>, PageError> {
    if marks
        .iter()
        .any(|mark| !valid_range(text, mark.start, mark.end))
    {
        return Err(PageError::InvalidTextRange);
    }
    normalize(&mut marks);
    if marks.len() > MAX_SPAN_MARKS_PER_BLOCK {
        return Err(PageError::TooManySpanMarks);
    }
    Ok(marks)
}

pub fn rebase_marks(marks: &mut Vec<SpanMark>, edit: TextEdit, new_len: u32) {
    for mark in marks.iter_mut() {
        mark.start = map_start(mark.start, edit).min(new_len);
        mark.end = map_end(mark.end, edit).min(new_len).max(mark.start);
    }
    marks.retain(|mark| mark.start < mark.end);
    normalize(marks);
}

fn boundary(text: &str, offset: u32) -> bool {
    let mut at = 0;
    if offset == 0 {
        return true;
    }
    for ch in text.chars() {
        at += ch.len_utf16() as u32;
        if at == offset {
            return true;
        }
        if at > offset {
            return false;
        }
    }
    false
}

pub fn valid_range(text: &str, start: u32, end: u32) -> bool {
    start < end && end <= utf16_len(text) && boundary(text, start) && boundary(text, end)
}

pub fn set_span_mark(
    marks: &mut Vec<SpanMark>,
    text: &str,
    start: u32,
    end: u32,
    kind: InlineMark,
    active: bool,
) -> Result<(), PageError> {
    if !valid_range(text, start, end) {
        return Err(PageError::InvalidTextRange);
    }
    if active {
        marks.push(SpanMark { start, end, kind });
    } else {
        let mut out = Vec::with_capacity(marks.len() + 1);
        for mark in marks.drain(..) {
            if mark.kind != kind || mark.end <= start || mark.start >= end {
                out.push(mark);
                continue;
            }
            if mark.start < start {
                out.push(SpanMark {
                    end: start,
                    ..mark.clone()
                });
            }
            if mark.end > end {
                out.push(SpanMark { start: end, ..mark });
            }
        }
        *marks = out;
    }
    normalize(marks);
    if marks.len() > MAX_SPAN_MARKS_PER_BLOCK {
        return Err(PageError::TooManySpanMarks);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_rebase_in_utf16_and_marks_split() {
        let edit = edit_between("a🦆bc", "++a🦆bc").unwrap();
        let mut anchor = RelativeAnchor { start: 1, end: 3 };
        rebase_anchor(&mut anchor, edit, utf16_len("++a🦆bc"));
        assert_eq!(anchor, RelativeAnchor { start: 3, end: 5 });

        let mut marks = vec![SpanMark {
            start: 0,
            end: 6,
            kind: InlineMark::Bold,
        }];
        set_span_mark(&mut marks, "++a🦆bc", 3, 5, InlineMark::Bold, false).unwrap();
        assert_eq!(
            marks,
            vec![
                SpanMark {
                    start: 0,
                    end: 3,
                    kind: InlineMark::Bold
                },
                SpanMark {
                    start: 5,
                    end: 6,
                    kind: InlineMark::Bold
                },
            ]
        );
        assert!(
            !valid_range("🦆", 0, 1),
            "a range cannot split a surrogate pair"
        );
    }
}
