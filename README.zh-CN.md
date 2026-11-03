# rust-airplay-ffmpeg

[`rust-airplay`](https://github.com/rust-airplay/rust-airplay) 的 FFmpeg 后端。

`rust-airplay` 接收 AirPlay 会话、解密、把编码帧交出来。本 crate 用 FFmpeg 命令行工具解码并播放：一个
`ffmpeg` 进程把音频解成 PCM，再由一个 sink 程序（默认 `ffplay`）播放。它实现核心库的 `Renderer`
trait，因此一个程序依赖核心库拿协议、依赖本 crate 拿播放。

[docs/ARCHITECTURE.zh-CN.md](docs/ARCHITECTURE.zh-CN.md) 解释为什么音频要包一层容器而视频不用、一帧在中途
变成了什么，以及 flush 能做什么、不能做什么。

## 依赖

**构建**只需要 Rust 工具链。本 crate 不链接任何 C 库，因此没有开发包要装，依赖树里也没有 `-sys` crate。

**运行**需要 `PATH` 上的 `ffmpeg`，以及默认 sink 所需的 `ffplay`；也可以在代码里通过
[`Options`](#选项) 或环境变量指定。FFmpeg 需要带 H.264、HEVC、ALAC、AAC-ELD 解码器——各发行版与包管理器
提供的构建都有这四个。

| 平台 | 安装什么 |
| --- | --- |
| Windows | MSYS2 UCRT64 的 `ffmpeg`，并把 `C:\msys64\ucrt64\bin` 加进 `PATH` |
| Debian / Ubuntu | `ffmpeg` |
| macOS | `brew install ffmpeg` |

## 用法

```toml
[dependencies]
rust-airplay = "0.3"
rust-airplay-ffmpeg = { git = "https://github.com/rust-airplay/rust-airplay-ffmpeg" }
```

```rust,no_run
use std::sync::Arc;

use rust_airplay::{Config, Renderer};

# async fn start() -> Result<(), Box<dyn std::error::Error>> {
let renderer: Arc<dyn Renderer> =
    Arc::new(rust_airplay_ffmpeg::FfmpegRenderer::new().expect("a machine with FFmpeg"));
let config = Config::new("Living room", [0x42; 32], renderer);
rust_airplay::run(config).await?;
# Ok(())
# }
```

## 选项

调用方能选的东西都在 `Options` 里，每个默认值都等于对应环境变量所指，所以 `Options::default` 的行为和
`FfmpegRenderer::new` 完全一样：

| 方法 | 含义 | 默认 |
| --- | --- | --- |
| `Options::new(audio_sink, video_sink)` | 直接写出两个 sink | -- |
| `.with_decoder(program)` | 解码音频的 `ffmpeg` | `ffmpeg` |
| `.with_audio_sink(sink)` | 播放音频的程序 | `ffplay -nodisp` |
| `.with_video_sink(sink)` | 显示视频的程序 | `ffplay` |
| `.with_pacing(bool)` / `.without_pacing()` | 是否执行 `set_pace` 要的间隔 | 开 |

sink 是一个程序加上参数，这些参数接在描述输入的那些参数之后。输入格式由后端给出——解码器读 Matroska，音频
sink 读 16 位 PCM，视频 sink 读 Annex-B——再附上 sink 自己的参数。于是同一个类型既能当播放器，也能当录制器：

```rust
use rust_airplay::{Renderer, VideoCodec};
use rust_airplay_ffmpeg::{FfmpegRenderer, Options, Program};

// 不播放，而是录制：PCM 写进 WAV，画面写进裸流。
let renderer = FfmpegRenderer::with_options(Options::new(
    Program::new("ffmpeg").args(["-y", "-f", "wav", "audio.wav"]),
    Program::new("ffmpeg").args(["-y", "-f", "rawvideo", "-pix_fmt", "yuv420p", "video.yuv"]),
))?;
# Ok::<(), rust_airplay::RenderError>(())
```

## 环境变量

| 变量 | 作用 |
| --- | --- |
| `AIRPLAY_FFMPEG` | 解码音频的 `ffmpeg` 可执行文件 |
| `AIRPLAY_FFMPEG_AUDIO_SINK` | 音频 sink，一整条命令行 |
| `AIRPLAY_FFMPEG_VIDEO_SINK` | 视频 sink，一整条命令行 |

`Options` 在构造时读取这些变量，所以想在代码里决定的程序应当用 `Options::new` 和 `with_*` 方法。
命令行按空白切分，不解析引号：带空格的路径写在代码里，不要写进变量。

## 它怎么处理流

- **音频**被包进一个 Matroska 流，解成 16 位 PCM，经过增益之后交给 sink 播放。裸 AAC-ELD 帧和 ALAC 帧既没有
  同步字、自身也不带长度，容器是告诉程序每帧在哪结束的唯一办法。
- **视频**按 push 的顺序以 Annex-B 写给 sink。起始码让解码器自己找画面边界，没有需要分帧的东西。
- **音量**是作用在 PCM 上的线性增益，所以拖动滑块在下一个采样就生效，而不是等到下一条流：放在解码器参数里的
  filter 在进程启动时就固定了。
- **flush** 换掉音频流的两个进程、视频流的解码器。解码器没法被告知忘掉已排队的东西，所以只能换掉；客户端在
  flush 之后发的那一帧关键帧，才让新的解码器正确而不只是空。
- **close** 关闭管道，也就是流结束：程序播完手上的内容再退出。两条流在 `Drop` 时也会停，因为会话结束时
  接收端可能直接丢掉流而不 close。

## 失败

`RenderError::Unavailable` 表示这台机器做不到：没有 `ffmpeg`、没有 `ffplay`、没有显示器、构建里没有
该解码器。`RenderError::Failed` 表示请求没能完成：程序拒绝了参数，或者在使用中停掉了。接收端会报告其中
任一种并继续服务；在后端内部重试的后端是在掩盖自己的失败。

## 测试

```sh
cargo test                      # 单元测试，不需要 FFmpeg
cargo test -- --ignored         # 需要 PATH 上有 ffmpeg 和 ffprobe
```

被忽略的测试解码仓库里自带的 fixture 并数结果：48 张画面、40 帧 AAC-ELD，以及测试现场生成的一条 ALAC 流——
它解出来的 PCM 必须和 `ffmpeg` 直接读它原来那个文件的结果逐字节相同。一个把所有帧都收下却一帧都不解码的
管道，只有数解码出的字节才能发现。[tests/fixtures/README.md](tests/fixtures/README.md) 记着每个 fixture 是
什么、从哪来。

## 许可

GPL-3.0-or-later。见 [LICENSE](LICENSE)。

英文：[README.md](README.md)。
