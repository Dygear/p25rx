#[macro_use]
extern crate serde_derive;

#[macro_use]
extern crate serde_json;

#[macro_use]
extern crate log;

extern crate arrayvec;
extern crate chan;
extern crate chrono;
extern crate clap;
extern crate collect_slice;
extern crate crossbeam;
extern crate demod_fm;
extern crate env_logger;
extern crate fnv;
extern crate imbe;
extern crate libc;
extern crate mio;
extern crate mio_extras;
extern crate moving_avg;
extern crate num;
extern crate p25;
extern crate p25_filts;
extern crate pool;
extern crate prctl;
extern crate rtlsdr_iq;
extern crate rtlsdr_mt;
extern crate serde;
extern crate slice_cast;
extern crate slice_mip;
extern crate static_decimate;
extern crate static_fir;
extern crate throttle;
extern crate uhttp_chunked_write;
extern crate uhttp_json_api;
extern crate uhttp_method;
extern crate uhttp_response_header;
extern crate uhttp_sse;
extern crate uhttp_status;
extern crate uhttp_uri;
extern crate uhttp_version;

use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    sync::mpsc::channel,
};

use anyhow::Result;
use clap::{App, Arg};
use env_logger::{Builder, Env};
use log::LevelFilter;
use rtlsdr_mt::TunerGains;

mod audio;
mod consts;
mod demod;
mod http;
mod hub;
mod policy;
mod recv;
mod replay;
mod sdr;
mod talkgroups;

use audio::{AudioOutput, AudioTask};
use consts::{BASEBAND_SAMPLE_RATE, SDR_SAMPLE_RATE};
use demod::DemodTask;
use hub::HubTask;
use policy::ReceiverPolicy;
use recv::RecvTask;
use replay::ReplayReceiver;
use sdr::{ControlTask, ReadTask};
use talkgroups::TalkgroupSelection;

fn main() -> Result<()> {
    let args = App::new("p25rx")
        .arg(
            Arg::with_name("verbose")
                .short("v")
                .help("enable verbose logging (pass twice to be extra verbose)")
                .multiple(true),
        )
        .arg(
            Arg::with_name("ppm")
                .short("p")
                .help("ppm frequency adjustment")
                .default_value("0")
                .value_name("PPM"),
        )
        .arg(
            Arg::with_name("audio")
                .short("a")
                .help("file/fifo for audio samples (f32le/8kHz/mono)")
                .value_name("FILE"),
        )
        .arg(
            Arg::with_name("gain")
                .short("g")
                .help("tuner gain (use -g list to see all options)")
                .value_name("GAIN"),
        )
        .arg(
            Arg::with_name("replay")
                .short("r")
                .help("replay from baseband samples in FILE")
                .value_name("FILE"),
        )
        .arg(
            Arg::with_name("write")
                .short("w")
                .help("write baseband samples to FILE (f32le/48kHz/mono)")
                .value_name("FILE"),
        )
        .arg(
            Arg::with_name("freq")
                .short("f")
                .help("frequency for initial control channel (Hz)")
                .value_name("FREQ"),
        )
        .arg(
            Arg::with_name("device")
                .short("d")
                .help("rtlsdr device index (use -d list to show all)")
                .default_value("0")
                .value_name("INDEX"),
        )
        .arg(
            Arg::with_name("bind")
                .short("b")
                .help("HTTP socket bind address")
                .default_value("0.0.0.0:8025")
                .value_name("BIND"),
        )
        .arg(
            Arg::with_name("nohop")
                .short("n")
                .long("nohop")
                .help("disable frequency hopping"),
        )
        .arg(
            Arg::with_name("pause")
                .long("pause-timeout")
                .help("time (sec) to wait for voice message to be resumed")
                .default_value("2.0")
                .value_name("TIME"),
        )
        .arg(
            Arg::with_name("watchdog")
                .long("watchdog-timeout")
                .help("time (sec) to wait for voice message to begin")
                .default_value("2.0")
                .value_name("TIME"),
        )
        .arg(
            Arg::with_name("tgselect")
                .long("tgselect-timeout")
                .help("time (sec) to collect talkgroups before making a selection")
                .default_value("1.0")
                .value_name("TIME"),
        )
        .get_matches();

    {
        let level = match args.occurrences_of("verbose") {
            0 => LevelFilter::Info,
            1 => LevelFilter::Debug,
            _ => LevelFilter::Trace,
        };

        Builder::from_env(Env::default()).filter(None, level).init();
    }

    let audio_out = || {
        let path = args.value_of("audio").expect("-a option is required");
        info!("writing audio frames to {}", path);

        AudioOutput::new(BufWriter::new(
            OpenOptions::new()
                .write(true)
                .open(path)
                .expect("unable to open audio output file"),
        ))
    };

    if let Some(path) = args.value_of("replay") {
        let mut stream = File::open(path).expect("unable to open replay file");
        let mut recv = ReplayReceiver::new(audio_out());

        recv.replay(&mut stream);

        return Ok(());
    }

    let ppm: i32 = args.value_of("ppm").unwrap().parse().expect("invalid ppm");

    let samples_file = args
        .value_of("write")
        .map(|path| File::create(path).expect("unable to open baseband file"));

    let dev: u32 = match args.value_of("device").unwrap() {
        "list" => {
            for (idx, name) in rtlsdr_mt::devices().enumerate() {
                println!("{}: {}", idx, name.to_str().unwrap());
            }

            return Ok(());
        }
        s => s.parse().expect("invalid device index"),
    };

    info!("opening RTL-SDR at index {}", dev);
    let (mut control, reader) = rtlsdr_mt::open(dev).expect("unable to open rtlsdr");

    match args.value_of("gain").expect("-g option is required") {
        "list" => {
            let mut gains = TunerGains::default();

            for g in control.tuner_gains(&mut gains) {
                println!("{}", g);
            }

            println!("auto");

            return Ok(());
        }
        "auto" => {
            info!("enabling hardware AGC");
            control.enable_agc().expect("unable to enable agc");
        }
        s => {
            let gain = s.parse().expect("invalid gain");
            info!("setting hardware gain to {:.1} dB", gain as f32 / 10.0);
            control.set_tuner_gain(gain).expect("unable to set gain");
        }
    }

    let hopping = !args.is_present("nohop");

    let pause = time_samples(
        args.value_of("pause")
            .unwrap()
            .parse()
            .expect("invalid pause timeout"),
    );
    let watchdog = time_samples(
        args.value_of("watchdog")
            .unwrap()
            .parse()
            .expect("invalid watchdog timeout"),
    );
    let tgselect = time_samples(
        args.value_of("tgselect")
            .unwrap()
            .parse()
            .expect("invalid tgselect timeout"),
    );

    info!("setting frequency offset to {} PPM", ppm);
    control.set_ppm(ppm).expect("unable to set ppm");
    control
        .set_sample_rate(SDR_SAMPLE_RATE)
        .expect("unable to set sample rate");

    let freq: u32 = args
        .value_of("freq")
        .expect("-f option is required")
        .parse()
        .expect("invalid frequency");
    info!("using control channel frequency {} Hz", freq);

    let addr = args
        .value_of("bind")
        .unwrap()
        .parse()
        .expect("invalid bind address");

    let (tx_ctl, rx_ctl) = channel();
    let (tx_recv, rx_recv) = channel();
    let (tx_read, rx_read) = channel();
    let (tx_audio, rx_audio) = channel();
    let (tx_hub, rx_hub) = mio_extras::channel::channel();

    let policy = ReceiverPolicy::new(tgselect, watchdog, pause);
    let talkgroups = TalkgroupSelection::default();

    info!("starting HTTP server at http://{}", addr);
    let mut hub = HubTask::new(rx_hub, tx_recv.clone(), &addr)?;
    let mut control = ControlTask::new(control, rx_ctl);
    let mut read = ReadTask::new(tx_read);
    let mut demod = DemodTask::new(rx_read, tx_hub.clone(), tx_recv.clone());
    let mut recv = RecvTask::new(
        rx_recv,
        tx_hub.clone(),
        tx_ctl.clone(),
        tx_audio.clone(),
        freq,
        hopping,
        policy,
        talkgroups,
    );
    let mut audio = AudioTask::new(audio_out(), rx_audio);

    crossbeam::scope(|scope| {
        scope.spawn(move || {
            prctl::set_name("hub").unwrap();
            hub.run();
        });

        scope.spawn(move || {
            prctl::set_name("controller").unwrap();
            control.run()
        });

        scope.spawn(move || {
            prctl::set_name("reader").unwrap();
            read.run(reader);
        });

        scope.spawn(move || {
            prctl::set_name("demod").unwrap();
            demod.run();
        });

        scope.spawn(move || {
            prctl::set_name("receiver").unwrap();

            if let Some(mut f) = samples_file {
                recv.run(|samples| {
                    f.write_all(unsafe { slice_cast::cast(samples) })
                        .expect("unable to write baseband");
                })
            }
            else {
                recv.run(|_| {})
            }
        });

        scope.spawn(move || {
            prctl::set_name("audio").unwrap();
            audio.run();
        });
    });

    Ok(())
}

/// Convert the given seconds into an amount of baseband samples.
fn time_samples(t: f32) -> usize {
    (t * BASEBAND_SAMPLE_RATE as f32) as usize
}
