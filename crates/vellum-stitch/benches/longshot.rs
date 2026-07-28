//! Long-shot stitching benchmark, ARCHITECTURE.md section 4.2 spec:
//! 101 frames of 900x700, 20 px scroll per frame, opaque and translucent.
//!
//! Deliberately not criterion: the interesting number is the wall clock a user
//! waits after releasing the finish key, on one run of the real workload. A
//! sampling harness would report per-`add` throughput, which hides the offline
//! rebuild and the single final `vstack` where the tail latency actually lives.
//!
//! Run with `cargo bench -p vellum-stitch`. Frame generation is excluded from
//! every reported figure; only `add` and `result` are timed.

use std::time::{Duration, Instant};

// The fixture generator is shared with the regression suite rather than
// duplicated. The bench only needs three of its helpers, so the rest are dead
// code *here* while still being live in `tests/longshot_regression.rs`.
#[path = "../tests/common/page.rs"]
#[allow(dead_code)]
mod page;

use page::{apply_translucency, page, viewport};
use vellum_core::image::Rgb8;
use vellum_stitch::{DEFAULT_MAX_DIFF, DEFAULT_MIN_SHIFT_PX, Stitcher};

const WIDTH: usize = 900;
const VIEW_H: usize = 700;
const STEP: usize = 20;
const FRAMES: usize = 101;

struct Timing {
    label: &'static str,
    add: Duration,
    result: Duration,
    height: usize,
    frames_used: usize,
    keyframe_bytes: usize,
    rebuilt: bool,
}

impl Timing {
    fn total(&self) -> Duration {
        self.add + self.result
    }

    fn report(&self) {
        let total = self.total();
        println!(
            "{:<12} total {:>8.1} ms   add {:>8.1} ms ({:>5.2} ms/frame)   finish {:>7.1} ms",
            self.label,
            total.as_secs_f64() * 1e3,
            self.add.as_secs_f64() * 1e3,
            self.add.as_secs_f64() * 1e3 / self.frames_used as f64,
            self.result.as_secs_f64() * 1e3,
        );
        println!(
            "{:<12} output {}x{} from {} frames, keyframes {:.1} MiB, offline rebuild {}",
            "",
            WIDTH,
            self.height,
            self.frames_used,
            self.keyframe_bytes as f64 / (1024.0 * 1024.0),
            if self.rebuilt { "used" } else { "not needed" },
        );
    }
}

/// Pre-generate every frame so image synthesis never lands in the timed region.
fn frames(translucent: bool) -> Vec<Rgb8> {
    let src = page(WIDTH, VIEW_H + STEP * (FRAMES - 1));
    (0..FRAMES)
        .map(|i| {
            let mut frame = viewport(&src, i * STEP, VIEW_H);
            if translucent {
                // Same mix as the translucency regression case: a terminal at
                // 72% over a dark wallpaper, which is where the Python baseline
                // slowed down the most.
                apply_translucency(&mut frame, 0.72, [24, 26, 38]);
            }
            frame
        })
        .collect()
}

fn run(label: &'static str, translucent: bool) -> Timing {
    let frames = frames(translucent);
    let mut stitcher = Stitcher::with_options(
        DEFAULT_MAX_DIFF,
        DEFAULT_MIN_SHIFT_PX,
        false,
        vellum_stitch::offline::KEYFRAME_MEMORY_LIMIT,
    );

    let started = Instant::now();
    for frame in &frames {
        stitcher.add(frame);
    }
    let add = started.elapsed();

    let frames_used = stitcher.frames_used;
    let keyframe_bytes = stitcher.keyframe_memory_used();

    let started = Instant::now();
    let result = stitcher.result().expect("stitch produced an image");
    let elapsed = started.elapsed();

    assert_eq!(result.image.width, WIDTH);
    assert_eq!(
        result.image.height,
        VIEW_H + STEP * (FRAMES - 1),
        "{label}: stitched height must match the scrolled distance"
    );

    Timing {
        label,
        add,
        result: elapsed,
        height: result.image.height,
        frames_used,
        keyframe_bytes,
        rebuilt: result.rebuilt,
    }
}

fn main() {
    println!(
        "long-shot stitch: {FRAMES} frames of {WIDTH}x{VIEW_H}, {STEP} px/frame \
         (expected output {WIDTH}x{})",
        VIEW_H + STEP * (FRAMES - 1)
    );
    println!();

    // One untimed warm-up so the first measured run does not pay for lazily
    // faulted heap pages or a cold rayon pool.
    let _ = run("warm-up", false);

    for timing in [run("opaque", false), run("translucent", true)] {
        timing.report();
    }
}
