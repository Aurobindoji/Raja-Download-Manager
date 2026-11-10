use std::error::Error;
use std::fmt;

pub mod app;
pub mod http;
pub mod part_file;
pub mod storage;
pub mod video_tools;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    start: u64,
    end_exclusive: u64,
}

impl ByteRange {
    pub fn start(self) -> u64 {
        self.start
    }

    pub fn end_exclusive(self) -> u64 {
        self.end_exclusive
    }

    pub fn len(self) -> u64 {
        self.end_exclusive - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end_exclusive
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    ZeroMinimumSegmentSize,
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroMinimumSegmentSize => {
                formatter.write_str("minimum segment size must be greater than zero")
            }
        }
    }
}

impl Error for PlanError {}

pub struct SegmentPlanner {
    pending: Vec<ByteRange>,
    min_segment_size: u64,
}

impl SegmentPlanner {
    pub fn new(total_size: u64, min_segment_size: u64) -> Result<Self, PlanError> {
        if min_segment_size == 0 {
            return Err(PlanError::ZeroMinimumSegmentSize);
        }

        let pending = if total_size == 0 {
            Vec::new()
        } else {
            vec![ByteRange {
                start: 0,
                end_exclusive: total_size,
            }]
        };

        Ok(Self {
            pending,
            min_segment_size,
        })
    }

    pub fn next_segment(&mut self) -> Option<ByteRange> {
        let (index, largest) = self
            .pending
            .iter()
            .enumerate()
            .max_by_key(|(_, range)| range.len())?;
        let largest = *largest;
        self.pending.swap_remove(index);

        let upper_half_len = largest.len() / 2 + largest.len() % 2;
        if upper_half_len < self.min_segment_size || upper_half_len == largest.len() {
            return Some(largest);
        }

        let split_at = largest.end_exclusive - upper_half_len;
        self.pending.push(ByteRange {
            start: largest.start,
            end_exclusive: split_at,
        });

        Some(ByteRange {
            start: split_at,
            end_exclusive: largest.end_exclusive,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ByteRange, PlanError, SegmentPlanner};

    #[test]
    fn rejects_a_zero_minimum_segment_size() {
        assert_eq!(
            SegmentPlanner::new(1, 0).err(),
            Some(PlanError::ZeroMinimumSegmentSize)
        );
    }

    #[test]
    fn empty_file_has_no_segments() {
        let mut planner = SegmentPlanner::new(0, 1).unwrap();
        assert_eq!(planner.next_segment(), None);
    }

    #[test]
    fn assigns_the_upper_half_of_the_largest_remaining_range() {
        let mut planner = SegmentPlanner::new(10, 2).unwrap();

        assert_eq!(
            planner.next_segment(),
            Some(ByteRange {
                start: 5,
                end_exclusive: 10
            })
        );
        assert_eq!(
            planner.next_segment(),
            Some(ByteRange {
                start: 2,
                end_exclusive: 5
            })
        );
        assert_eq!(
            planner.next_segment(),
            Some(ByteRange {
                start: 0,
                end_exclusive: 2
            })
        );
        assert_eq!(planner.next_segment(), None);
    }

    #[test]
    fn planned_segments_cover_each_file_without_gaps_or_overlaps() {
        for total_size in 0..=512 {
            for min_segment_size in 1..=16 {
                let mut planner = SegmentPlanner::new(total_size, min_segment_size).unwrap();
                let mut segments = Vec::new();

                while let Some(segment) = planner.next_segment() {
                    segments.push(segment);
                }

                segments.sort_by_key(|segment| segment.start());

                let mut next_byte = 0;
                for segment in segments {
                    assert!(!segment.is_empty());
                    assert_eq!(segment.start(), next_byte);
                    next_byte = segment.end_exclusive();
                }
                assert_eq!(next_byte, total_size);
            }
        }
    }
}
