//! Criterion benchmarks for RustDesk client performance-critical paths.
//!
//! Run with: `cargo bench --features linux-pkg-config`

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::sync::Arc;
use std::time::Instant;

// ---------------------------------------------------------------------------
// 1. Video QoS benchmarks
// ---------------------------------------------------------------------------

mod video_qos_benches {
    use super::*;
    use librustdesk::video_qos::{RttCalculator, VideoQoS};

    /// Set up a VideoQoS with one connected user and one display, ready for
    /// benchmark iterations.
    fn setup_qos() -> VideoQoS {
        let mut qos = VideoQoS::default();
        qos.on_connection_open(1);
        qos.new_display("bench_display".to_string());
        qos.set_support_changing_quality("bench_display", true);
        qos
    }

    pub fn bench_user_network_delay(c: &mut Criterion) {
        let mut group = c.benchmark_group("video_qos/user_network_delay");

        // Benchmark with varying delay values representing different network
        // conditions.
        for &delay_ms in &[20u32, 80, 150, 300, 600] {
            group.bench_with_input(
                BenchmarkId::from_parameter(format!("{}ms", delay_ms)),
                &delay_ms,
                |b, &delay| {
                    let mut qos = setup_qos();
                    b.iter(|| {
                        qos.user_network_delay(1, black_box(delay));
                    });
                },
            );
        }
        group.finish();
    }

    pub fn bench_adjust_ratio(c: &mut Criterion) {
        let mut group = c.benchmark_group("video_qos/adjust_ratio");

        group.bench_function("dynamic_screen", |b| {
            let mut qos = setup_qos();
            // Seed some delay data so ratio adjustment has something to work with.
            for _ in 0..5 {
                qos.user_network_delay(1, 50);
            }
            b.iter(|| {
                qos.adjust_ratio(black_box(true));
            });
        });

        group.bench_function("static_screen", |b| {
            let mut qos = setup_qos();
            for _ in 0..5 {
                qos.user_network_delay(1, 50);
            }
            b.iter(|| {
                qos.adjust_ratio(black_box(false));
            });
        });

        group.finish();
    }

    pub fn bench_rtt_calculator(c: &mut Criterion) {
        let mut group = c.benchmark_group("video_qos/rtt_calculator");

        group.bench_function("update", |b| {
            let mut calc = RttCalculator::default();
            let mut seq = 0u32;
            b.iter(|| {
                seq = seq.wrapping_add(1);
                // Simulate realistic jittery delays: base 30ms +/- variation.
                let delay = 30 + (seq % 40);
                calc.update(black_box(delay));
            });
        });

        group.bench_function("update_then_get_rtt", |b| {
            let mut calc = RttCalculator::default();
            // Pre-fill enough samples so get_rtt returns a value.
            for i in 0..60 {
                calc.update(30 + (i % 20));
            }
            let mut seq = 60u32;
            b.iter(|| {
                seq = seq.wrapping_add(1);
                let delay = 30 + (seq % 40);
                calc.update(black_box(delay));
                black_box(calc.get_rtt());
            });
        });

        group.finish();
    }

    pub fn bench_spf(c: &mut Criterion) {
        c.bench_function("video_qos/spf", |b| {
            let qos = setup_qos();
            b.iter(|| {
                black_box(qos.spf());
            });
        });
    }
}

// ---------------------------------------------------------------------------
// 2. FEC benchmarks
// ---------------------------------------------------------------------------

mod fec_benches {
    use super::*;
    use librustdesk::transport::fec::{
        fragment_frame, FecEncoder, FrameReassembler, VideoPacket, VideoPacketHeader,
        FRAME_TYPE_I, FRAME_TYPE_P, MAX_PAYLOAD_SIZE,
    };

    /// Build a pseudo-random byte buffer of the given size.
    /// Uses a simple LCG so the data is deterministic but not all-zeros
    /// (avoids any optimizations that might short-circuit on zero buffers).
    fn make_frame_data(size: usize) -> Vec<u8> {
        let mut data = vec![0u8; size];
        let mut state: u64 = 0xDEAD_BEEF;
        for byte in data.iter_mut() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            *byte = (state >> 33) as u8;
        }
        data
    }

    pub fn bench_fragment_frame(c: &mut Criterion) {
        let mut group = c.benchmark_group("fec/fragment_frame");

        for &size in &[1024usize, 64 * 1024, 1024 * 1024] {
            let label = if size < 1024 * 1024 {
                format!("{}KB", size / 1024)
            } else {
                format!("{}MB", size / (1024 * 1024))
            };
            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(BenchmarkId::from_parameter(&label), &size, |b, &sz| {
                let data = make_frame_data(sz);
                b.iter(|| {
                    black_box(fragment_frame(
                        black_box(&data),
                        1,
                        FRAME_TYPE_P,
                        1000,
                        0,
                        0,
                    ));
                });
            });
        }

        group.finish();
    }

    pub fn bench_generate_parity(c: &mut Criterion) {
        let mut group = c.benchmark_group("fec/generate_parity");

        // Test with different FEC group sizes (number of data packets).
        for &group_size in &[4u16, 10, 48] {
            let label = format!("{}_packets", group_size);

            // Create data packets with realistic payload sizes.
            let packets: Vec<VideoPacket> = (0..group_size)
                .map(|i| VideoPacket {
                    header: VideoPacketHeader {
                        sequence: i as u32,
                        frame_number: 1,
                        fragment_index: i,
                        fragment_count: group_size,
                        frame_type: FRAME_TYPE_P,
                        fec_group_id: 0,
                        fec_data_count: group_size as u8,
                        fec_parity_count: 0,
                        timestamp_ms: 1000,
                        payload_length: MAX_PAYLOAD_SIZE as u32,
                    },
                    payload: make_frame_data(MAX_PAYLOAD_SIZE),
                })
                .collect();

            let total_bytes = group_size as u64 * MAX_PAYLOAD_SIZE as u64;
            group.throughput(Throughput::Bytes(total_bytes));
            group.bench_with_input(BenchmarkId::from_parameter(&label), &packets, |b, pkts| {
                b.iter(|| {
                    black_box(FecEncoder::generate_parity(
                        black_box(pkts),
                        0,
                        pkts.len() as u32,
                    ));
                });
            });
        }

        group.finish();
    }

    pub fn bench_reassembly(c: &mut Criterion) {
        let mut group = c.benchmark_group("fec/reassembly");

        // Benchmark reassembling complete frames of various sizes.
        for &size in &[1024usize, 64 * 1024, 1024 * 1024] {
            let label = if size < 1024 * 1024 {
                format!("{}KB", size / 1024)
            } else {
                format!("{}MB", size / (1024 * 1024))
            };
            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(BenchmarkId::from_parameter(&label), &size, |b, &sz| {
                // Pre-fragment the frame so we benchmark only reassembly.
                let data = make_frame_data(sz);
                let fragmented = fragment_frame(&data, 0, FRAME_TYPE_P, 1000, 0, 0);

                b.iter(|| {
                    let mut reassembler = FrameReassembler::new(16);
                    for pkt in &fragmented.packets {
                        if let Some(frame) = reassembler.reassemble(pkt) {
                            black_box(&frame);
                        }
                    }
                });
            });
        }

        group.finish();
    }

    pub fn bench_header_serialization(c: &mut Criterion) {
        let mut group = c.benchmark_group("fec/header_serialization");

        let header = VideoPacketHeader {
            sequence: 42,
            frame_number: 7,
            fragment_index: 3,
            fragment_count: 10,
            frame_type: FRAME_TYPE_I,
            fec_group_id: 1,
            fec_data_count: 10,
            fec_parity_count: 1,
            timestamp_ms: 123456,
            payload_length: 1376,
        };

        group.bench_function("to_bytes", |b| {
            b.iter(|| {
                black_box(black_box(&header).to_bytes());
            });
        });

        let bytes = header.to_bytes();
        group.bench_function("from_bytes", |b| {
            b.iter(|| {
                black_box(VideoPacketHeader::from_bytes(black_box(&bytes)));
            });
        });

        group.finish();
    }
}

// ---------------------------------------------------------------------------
// 3. Cursor prediction benchmarks
// ---------------------------------------------------------------------------

mod cursor_prediction_benches {
    use super::*;
    use librustdesk::cursor_prediction::CursorPredictor;

    pub fn bench_on_local_mouse_move(c: &mut Criterion) {
        c.bench_function("cursor_prediction/on_local_mouse_move", |b| {
            let predictor = CursorPredictor::new(true);
            let mut x = 0i32;
            b.iter(|| {
                x = x.wrapping_add(1);
                predictor.on_local_mouse_move(black_box(x), black_box(x * 2));
            });
        });
    }

    pub fn bench_get_render_position(c: &mut Criterion) {
        let mut group = c.benchmark_group("cursor_prediction/get_render_position");

        group.bench_function("with_prediction", |b| {
            let predictor = CursorPredictor::new(true);
            predictor.on_local_mouse_move(500, 300);
            b.iter(|| {
                black_box(predictor.get_render_position());
            });
        });

        group.bench_function("with_server_confirmed", |b| {
            let predictor = CursorPredictor::new(true);
            predictor.on_local_mouse_move(500, 300);
            predictor.on_server_cursor(505, 305); // within snap threshold
            b.iter(|| {
                black_box(predictor.get_render_position());
            });
        });

        group.finish();
    }

    pub fn bench_on_server_cursor(c: &mut Criterion) {
        let mut group = c.benchmark_group("cursor_prediction/on_server_cursor");

        group.bench_function("snap_reconciliation", |b| {
            let predictor = CursorPredictor::new(true);
            let mut x = 100i32;
            b.iter(|| {
                x = x.wrapping_add(1);
                // Set a fresh prediction, then reconcile with a nearby server pos.
                predictor.on_local_mouse_move(x, x * 2);
                predictor.on_server_cursor(black_box(x + 5), black_box(x * 2 + 5));
            });
        });

        group.bench_function("lerp_reconciliation", |b| {
            let predictor = CursorPredictor::new(true);
            let mut x = 100i32;
            b.iter(|| {
                x = x.wrapping_add(1);
                // Set a fresh prediction, then reconcile with a distant server pos
                // (> 20px away) to trigger lerp.
                predictor.on_local_mouse_move(x, x * 2);
                predictor.on_server_cursor(black_box(x + 100), black_box(x * 2 + 100));
            });
        });

        group.finish();
    }
}

// ---------------------------------------------------------------------------
// 4. Frame buffer benchmarks
// ---------------------------------------------------------------------------

mod frame_buffer_benches {
    use super::*;
    use librustdesk::frame_buffer::{CapturedFrame, FrameBuffer};
    use scrap::Pixfmt;

    /// Build a 1920x1080 BGRA frame (~8MB).
    fn make_1080p_frame(display_idx: usize) -> CapturedFrame {
        let width = 1920;
        let height = 1080;
        let stride = width * 4;
        CapturedFrame {
            data: vec![0u8; stride * height],
            width,
            height,
            stride,
            pixfmt: Pixfmt::BGRA,
            capture_time: Instant::now(),
            display_idx,
        }
    }

    pub fn bench_store(c: &mut Criterion) {
        c.bench_function("frame_buffer/store_1080p", |b| {
            let buf = FrameBuffer::new();
            b.iter(|| {
                buf.store(black_box(make_1080p_frame(0)));
            });
        });
    }

    pub fn bench_take(c: &mut Criterion) {
        c.bench_function("frame_buffer/take_1080p", |b| {
            let buf = FrameBuffer::new();
            b.iter_custom(|iters| {
                // Pre-store frames so we measure take latency, not "take on empty".
                let start = Instant::now();
                for _ in 0..iters {
                    buf.store(make_1080p_frame(0));
                    black_box(buf.take());
                }
                start.elapsed()
            });
        });
    }

    pub fn bench_concurrent_store_take(c: &mut Criterion) {
        c.bench_function("frame_buffer/concurrent_store_take", |b| {
            b.iter_custom(|iters| {
                let buf = Arc::new(FrameBuffer::new());
                let buf_producer = Arc::clone(&buf);
                let buf_consumer = Arc::clone(&buf);
                let iters = iters as usize;

                let start = Instant::now();

                // Producer thread: store frames as fast as possible.
                let producer = std::thread::spawn(move || {
                    for i in 0..iters {
                        buf_producer.store(make_1080p_frame(i));
                    }
                });

                // Consumer thread: take frames as fast as possible.
                let consumer = std::thread::spawn(move || {
                    let mut taken = 0usize;
                    while taken < iters {
                        if buf_consumer.take().is_some() {
                            taken += 1;
                        }
                        std::hint::spin_loop();
                    }
                });

                producer.join().unwrap();
                consumer.join().unwrap();

                start.elapsed()
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Criterion group and main
// ---------------------------------------------------------------------------

criterion_group!(
    video_qos,
    video_qos_benches::bench_user_network_delay,
    video_qos_benches::bench_adjust_ratio,
    video_qos_benches::bench_rtt_calculator,
    video_qos_benches::bench_spf,
);

criterion_group!(
    fec,
    fec_benches::bench_fragment_frame,
    fec_benches::bench_generate_parity,
    fec_benches::bench_reassembly,
    fec_benches::bench_header_serialization,
);

criterion_group!(
    cursor_prediction,
    cursor_prediction_benches::bench_on_local_mouse_move,
    cursor_prediction_benches::bench_get_render_position,
    cursor_prediction_benches::bench_on_server_cursor,
);

criterion_group!(
    frame_buffer,
    frame_buffer_benches::bench_store,
    frame_buffer_benches::bench_take,
    frame_buffer_benches::bench_concurrent_store_take,
);

criterion_main!(video_qos, fec, cursor_prediction, frame_buffer);
