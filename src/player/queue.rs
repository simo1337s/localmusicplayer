//! Play queue: a context (album/playlist) with shuffle/repeat plus a user "up next" list.

use std::collections::VecDeque;

use rand::seq::SliceRandom;

use crate::model::{RepeatMode, Track};

#[derive(Default, Clone)]
pub struct Queue {
    /// Tracks of the current context in their original order.
    tracks: Vec<Track>,
    /// Play order as indices into `tracks`.
    order: Vec<usize>,
    /// Position in `order` of the current context track.
    pos: Option<usize>,
    /// Manually queued tracks, played before the context continues.
    up_next: VecDeque<Track>,
    /// The playing track (may come from `up_next`).
    current: Option<Track>,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub context_name: String,
}

impl Queue {
    pub fn set_context(&mut self, tracks: Vec<Track>, start: usize, name: String) -> Option<Track> {
        self.tracks = tracks;
        self.context_name = name;
        if self.tracks.is_empty() {
            self.order.clear();
            self.pos = None;
            self.current = None;
            return None;
        }
        let start = start.min(self.tracks.len() - 1);
        self.build_order(start);
        self.current = Some(self.tracks[start].clone());
        self.current.clone()
    }

    fn build_order(&mut self, current: usize) {
        self.order = (0..self.tracks.len()).collect();
        if self.shuffle {
            self.order.retain(|&i| i != current);
            self.order.shuffle(&mut rand::rng());
            self.order.insert(0, current);
            self.pos = Some(0);
        } else {
            self.pos = Some(current);
        }
    }

    pub fn current(&self) -> Option<&Track> {
        self.current.as_ref()
    }

    /// Index into the original context of the current context track.
    fn current_context_index(&self) -> Option<usize> {
        self.pos.and_then(|p| self.order.get(p).copied())
    }

    pub fn set_shuffle(&mut self, on: bool) {
        if self.shuffle == on {
            return;
        }
        self.shuffle = on;
        if let Some(cur) = self.current_context_index() {
            self.build_order(cur);
        }
    }

    /// What `advance(false)` would return, without changing state. Used for preloading.
    pub fn peek_next(&self) -> Option<&Track> {
        if self.repeat == RepeatMode::One {
            return self.current.as_ref();
        }
        if let Some(t) = self.up_next.front() {
            return Some(t);
        }
        let next = self.pos.map_or(0, |p| p + 1);
        if let Some(&i) = self.order.get(next) {
            return self.tracks.get(i);
        }
        if self.repeat == RepeatMode::All {
            return self.order.first().and_then(|&i| self.tracks.get(i));
        }
        None
    }

    /// Moves to the next track. `user` is true for an explicit "next" press, which
    /// skips over repeat-one.
    pub fn advance(&mut self, user: bool) -> Option<Track> {
        if !user && self.repeat == RepeatMode::One && self.current.is_some() {
            return self.current.clone();
        }
        if let Some(t) = self.up_next.pop_front() {
            self.current = Some(t);
            return self.current.clone();
        }
        let next = self.pos.map_or(0, |p| p + 1);
        if next < self.order.len() {
            self.pos = Some(next);
        } else if self.repeat != RepeatMode::Off && !self.order.is_empty() {
            if self.shuffle {
                // Reshuffle for the next round.
                self.order.shuffle(&mut rand::rng());
            }
            self.pos = Some(0);
        } else {
            self.current = None;
            return None;
        }
        self.current = self.current_context_index().map(|i| self.tracks[i].clone());
        self.current.clone()
    }

    pub fn previous(&mut self) -> Option<Track> {
        match self.pos {
            Some(p) if p > 0 => self.pos = Some(p - 1),
            Some(_) if self.repeat == RepeatMode::All && !self.order.is_empty() => {
                self.pos = Some(self.order.len() - 1)
            }
            _ => return self.current.clone(),
        }
        self.current = self.current_context_index().map(|i| self.tracks[i].clone());
        self.current.clone()
    }

    pub fn enqueue(&mut self, tracks: Vec<Track>) {
        self.up_next.extend(tracks);
    }

    pub fn play_next(&mut self, tracks: Vec<Track>) {
        for t in tracks.into_iter().rev() {
            self.up_next.push_front(t);
        }
    }

    /// Upcoming tracks in play order: manual queue first, then the rest of the context.
    pub fn upcoming(&self, limit: usize) -> Vec<Track> {
        let rest = self.pos.map_or(0, |p| p + 1);
        self.up_next
            .iter()
            .cloned()
            .chain(self.order.iter().skip(rest).map(|&i| self.tracks[i].clone()))
            .take(limit)
            .collect()
    }

    pub fn up_next_len(&self) -> usize {
        self.up_next.len()
    }

    /// Jumps to entry `index` of `upcoming()`.
    pub fn jump_to(&mut self, index: usize) -> Option<Track> {
        if index < self.up_next.len() {
            self.up_next.drain(..index);
            return self.advance(true);
        }
        let ctx_offset = index - self.up_next.len();
        self.up_next.clear();
        let rest = self.pos.map_or(0, |p| p + 1);
        let target = rest + ctx_offset;
        if target >= self.order.len() {
            return None;
        }
        self.pos = Some(target);
        self.current = self.current_context_index().map(|i| self.tracks[i].clone());
        self.current.clone()
    }

    /// Removes entry `index` of `upcoming()`.
    pub fn remove(&mut self, index: usize) {
        if index < self.up_next.len() {
            self.up_next.remove(index);
            return;
        }
        let rest = self.pos.map_or(0, |p| p + 1);
        let target = rest + index - self.up_next.len();
        if target < self.order.len() {
            self.order.remove(target);
        }
    }

    pub fn clear_upcoming(&mut self) {
        self.up_next.clear();
        let keep = self.pos.map_or(0, |p| p + 1);
        self.order.truncate(keep);
    }

    pub fn is_empty(&self) -> bool {
        self.current.is_none()
    }

    /// Replaces a track (e.g. after resolving an Apple Music song) everywhere in the queue.
    pub fn replace_track(&mut self, old_id: &str, new: &Track) {
        for t in self.tracks.iter_mut().chain(self.up_next.iter_mut()).chain(self.current.iter_mut()) {
            if t.id == old_id {
                *t = new.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Source;

    fn tracks(n: usize) -> Vec<Track> {
        (0..n)
            .map(|i| Track {
                id: format!("t{i}"),
                source: Source::Local,
                title: format!("T{i}"),
                artist: String::new(),
                album: String::new(),
                duration_ms: 0,
                track_no: None,
                art: None,
                uri: String::new(),
                added_at: 0,
            })
            .collect()
    }

    fn id(t: Option<Track>) -> String {
        t.map(|t| t.id).unwrap_or_default()
    }

    #[test]
    fn linear_playback_and_end() {
        let mut q = Queue::default();
        assert_eq!(id(q.set_context(tracks(3), 1, "ctx".into())), "t1");
        assert_eq!(id(q.peek_next().cloned()), "t2");
        assert_eq!(id(q.advance(false)), "t2");
        assert_eq!(q.advance(false), None);
    }

    #[test]
    fn repeat_modes() {
        let mut q = Queue::default();
        q.set_context(tracks(2), 1, String::new());
        q.repeat = RepeatMode::One;
        assert_eq!(id(q.advance(false)), "t1");
        // An explicit "next" leaves the repeated track (and wraps since repeat applies).
        assert_eq!(id(q.advance(true)), "t0");
        q.repeat = RepeatMode::All;
        assert_eq!(id(q.advance(false)), "t1");
        assert_eq!(id(q.advance(false)), "t0");
        assert_eq!(id(q.previous()), "t1");
    }

    #[test]
    fn up_next_goes_first() {
        let mut q = Queue::default();
        q.set_context(tracks(3), 0, String::new());
        let extra = tracks(5).split_off(3); // t3, t4
        q.enqueue(vec![extra[0].clone()]);
        q.play_next(vec![extra[1].clone()]);
        let upcoming: Vec<String> = q.upcoming(10).into_iter().map(|t| t.id).collect();
        assert_eq!(upcoming, vec!["t4", "t3", "t1", "t2"]);
        assert_eq!(id(q.advance(false)), "t4");
        assert_eq!(id(q.advance(false)), "t3");
        assert_eq!(id(q.advance(false)), "t1");
    }

    #[test]
    fn shuffle_keeps_current_first_and_covers_all() {
        let mut q = Queue::default();
        q.shuffle = true;
        q.set_context(tracks(20), 7, String::new());
        assert_eq!(id(q.current().cloned()), "t7");
        let mut seen: Vec<String> = q.upcoming(100).into_iter().map(|t| t.id).collect();
        seen.push("t7".into());
        seen.sort();
        let mut all: Vec<String> = tracks(20).into_iter().map(|t| t.id).collect();
        all.sort();
        assert_eq!(seen, all);
        q.set_shuffle(false);
        assert_eq!(id(q.peek_next().cloned()), "t8");
    }

    #[test]
    fn jump_and_remove() {
        let mut q = Queue::default();
        q.set_context(tracks(5), 0, String::new());
        q.remove(0); // removes t1
        assert_eq!(id(q.jump_to(1)), "t3");
        assert_eq!(id(q.advance(false)), "t4");
    }
}
