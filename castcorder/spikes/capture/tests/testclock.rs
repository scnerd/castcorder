//! ADR 0003 seam check: the pipeline clock can be replaced by a
//! deterministic GstTestClock, so a "live" source produces buffers only when
//! the test advances time. This is how a time port would drive GStreamer.

use gst::prelude::*;

#[test]
fn live_source_driven_by_test_clock() {
    gst::init().unwrap();
    let pipeline = gst::parse::launch(
        "audiotestsrc is-live=true samplesperbuffer=480 ! audio/x-raw,rate=48000,channels=1,format=S16LE \
         ! appsink name=s sync=false async=false",
    )
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let clock = gst_check::TestClock::new();
    pipeline.use_clock(Some(&clock));
    let sink = pipeline
        .by_name("s")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();

    let mut pts = Vec::new();
    for _ in 0..10 {
        clock.wait_for_next_pending_id();
        assert!(clock.crank());
        let s = sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .expect("buffer after crank");
        pts.push(s.buffer().unwrap().pts().unwrap().mseconds());
    }
    // No buffer arrives while the test clock stands still.
    assert!(sink.try_pull_sample(gst::ClockTime::from_mseconds(200)).is_none());
    pipeline.set_state(gst::State::Null).unwrap();
    println!("pts (ms) under TestClock: {pts:?}");
    assert_eq!(pts, (0..10).map(|i| i * 10).collect::<Vec<u64>>());
}
