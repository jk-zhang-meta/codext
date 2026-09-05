# 发布 codext

**默认在自己的机器上编,不要用 GitHub Actions。** `.github/workflows/codext-release.yml`
仍然留着,但只接受手动触发,理由是实测数据:

| 编译机 | 核心 / 内存 | 实测耗时 |
| --- | --- | --- |
| GitHub `ubuntu-latest` | 4 vCPU / 16 GB | **36 分 23 秒** |
| GitHub `macos-14` (M1) | 3 vCPU / 7 GB | 整轮 **1 小时 2 分** |
| 自己的 Linux 开发机 | 32 核 / 61 GB | 几分钟 |
| 自己的 M3 Max | 16 核 | 几分钟 |

GitHub 那两台是共享的小规格虚拟机,`cargo` 已经在满并行跑了,慢的是核心数本身;
而 `publish` 是 `needs: build`,两条里慢的那条会把整个发布卡住。

## 目标只有两个

```
x86_64-unknown-linux-gnu      在 Linux 机器上编
aarch64-apple-darwin          在 Apple Silicon 机器上编
```

没有 Windows 目标,也没有 arm64 Linux——`ags` 只找这两个包。

**macOS 不能从 Linux 交叉编译**:要 Apple 的 SDK(只随 Xcode 分发)、要能产出
Mach-O 的链接器,而且依赖树里的 C 构建脚本(`openssl-sys`、`ring` 等)也要目标
平台的头文件。凑 `osxcross` 那套是可能的,但编出来的东西没法在本机测。

## 两条铁律

1. **不要在 OneDrive 同步目录里编。** `target/` 有好几个 GB,会被同步回去。
   Linux 端先复制到 `~/.agent-work/runtime/`;Mac 端直接 `git clone` 到
   一个独立目录(而且 Mac 上的 OneDrive 是按需占位符,那份 `codext` 是 0 字节,
   本来也编不了)。

2. **必须走 `~/.cargo/bin` 的 rustup shim。** 仓库用 `rust-toolchain.toml` 钉死
   `1.95.0`。按绝对路径直接调某个工具链的 `cargo` 会**绕过版本解析**,拿一个
   更新的 nightly 去编——`cargo check` 和测试都会过,直到 `codex-tui` 在
   `semicolon_in_expressions_from_macros` 上炸。

## 步骤

### 1. 打标签

标签就是上游 Codex 的版本号,不加任何后缀——`codext --version` 报的就是那个版本,
因为它**就是**那个版本外加一个凭据池。所以同一个上游基线上的第二次构建**复用同一个
标签**,强推即可。`ags` 靠资源的 sha256 摘要分辨新旧,不看标签。

```
git tag -f v0.146.0 && git push -f origin v0.146.0
```

### 2. Linux 包

```
rsync -a --delete --exclude target/ <codext>/ ~/.agent-work/runtime/codext-<hash>/
cd ~/.agent-work/runtime/codext-<hash>/codex-rs
PATH=$HOME/.cargo/bin:$PATH cargo build --release --target x86_64-unknown-linux-gnu -p codex-cli
```

### 3. macOS 包

```
ssh <apple-silicon-host>
git clone --depth 1 --branch <tag> <repo> ~/codext-build
cd ~/codext-build/codex-rs
PATH=$HOME/.cargo/bin:$PATH cargo build --release --target aarch64-apple-darwin -p codex-cli
```

首次会自动装 1.95.0 工具链和目标。需要 Xcode command line tools。

### 4. 打包——这是和 `ags update` 的契约

包名和里面那个可执行文件的名字都不能改,改了等于让所有机器更新不到:

```
staging=$(mktemp -d)
cp "target/<target>/release/codex" "$staging/codext"     # 上游二进制叫 codex
chmod 755 "$staging/codext"
tar -czf "codext-<target>.tar.gz" -C "$staging" codext
shasum -a 256 "codext-<target>.tar.gz" > "codext-<target>.tar.gz.sha256"
```

改名放在打包这一步而不是 `Cargo.toml` 里,是为了让 `[[bin]]` 和上游保持一字不差,
合并时永远不冲突。

### 5. 上传

```
gh release create <tag> --title <tag> --notes "…" codext-*.tar.gz*
  || gh release upload <tag> --clobber codext-*.tar.gz*
```

### 6. 各机器更新

```
ags update
```

它按 musl → gnu 的顺序找本机能用的包。私有仓库时会带 token 并用 API 的 asset URL
(`/releases/assets/<id>` + `Accept: application/octet-stream`)——**不能用
`browser_download_url`**,那个会跳到 CDN,curl 跨主机会把 Authorization 头丢掉。

## 产物为什么这么大

`[profile.release]` 里 `strip = false`、`debug = "line-tables-only"`,所以二进制
带着调试信息,Linux 包 342 MB、macOS 包 122 MB,每台机器每次更新都要下这么多。
链接那一步基本是单线程且吃 I/O,也是耗时大头。

想变小变快,在**构建命令**上加环境变量覆盖即可,不要去动工作区的 `Cargo.toml`
(那个文件是全仓上游改动最频繁的之一):

```
CARGO_PROFILE_RELEASE_DEBUG=none CARGO_PROFILE_RELEASE_STRIP=symbols cargo build …
```

代价是 panic 的 backtrace 会失去行号和函数名。
