# 跨平台打包与安装

`geometry_pyramid` 的发布工作流为每个 PostgreSQL 主版本、操作系统和 CPU 架构单独构建原生扩展。每份产物包含动态库、扩展控制文件、安装与升级 SQL、安装脚本和 SHA-256 校验信息。

本页说明多平台工作流生成的安装包格式。2026-11-09 发布的 `v0.1.1` Ubuntu 压缩包使用较早的系统目录布局，安装时以该 Release 的说明为准；新工作流生成的包包含下文的 `install.sh` 或 `install.ps1`。

## 构建矩阵

矩阵由 [package-targets.json](../.github/package-targets.json) 管理，PostgreSQL 版本同时与 [Cargo.toml](../Cargo.toml) 的 `pgNN` feature 校验。完整运行包含 20 个独立作业。

| 构建系统 | CPU 架构 | PostgreSQL 主版本 | 安装包 |
| --- | --- | --- | --- |
| Ubuntu 24.04 | x86_64 | 15、16、17、18 | `.tar.gz` |
| Ubuntu 24.04 | aarch64 / ARM64 | 15、16、17、18 | `.tar.gz` |
| macOS 15 | x86_64 / Intel | 15、16、17、18 | `.tar.gz` |
| macOS 15 | aarch64 / Apple Silicon | 15、16、17、18 | `.tar.gz` |
| Windows Server 2022 | x86_64 | 15、16、17、18 | `.zip` |

系统名称表示实际构建和验证环境。选择安装包时，应匹配 PostgreSQL **服务器**的主版本、操作系统和 CPU 架构；同一个主版本的不同小版本也需要满足平台与运行库兼容条件。Linux 包记录构建产物所需的最低 glibc 版本，安装器会检查目标机器。其他 Linux 发行版应增加独立的原生构建与安装验证目标。

从 `0.1.3` 起仅支持 PG15–18：移除 PG13、PG14 的 Cargo feature、CI 手动选项和构建目标，新打包工具与安装器也会拒绝这些主版本。[v0.1.2 的 30 份历史安装包及校验文件](https://github.com/mapseekai/geometry_pyramid/releases/tag/v0.1.2)保持原样，但 PG13、PG14 不再属于当前支持范围。旧服务器需先按 PostgreSQL 自身的升级流程迁移到 15–18，不能将其他主版本的扩展库直接复制过去。PG19 尚未纳入本项目依赖和验收矩阵。

PostGIS 至少需要 3.1，并且构建时启用 MVT/protobuf 支持。安装包不捆绑 PostgreSQL、PostGIS 或它们的运行库。

Windows 包用于原生 Windows PostgreSQL。macOS 的 PG15 动态库名为 `geometry_pyramid.so`，PG16–18 为 `geometry_pyramid.dylib`；安装器按包内信息选择正确文件。

## 触发构建与获取产物

在 GitHub 的 **Actions → Verify and release** 页面运行工作流：

1. `platform=all`、`postgres=all`：构建全部 20 个组合。
2. 指定一个平台或一个 PG 主版本：仅验证所选组合，适合调试单个平台。
3. 每个成功作业提供独立的 `geometry-pyramid-pgN-<platform>-<arch>` Artifact。
4. 所有选择的组合成功后，`geometry-pyramid-release-set` Artifact 汇总安装包、`release-manifest.json` 和总校验文件 `SHA256SUMS`。

Pull Request 自动验证完整矩阵。推送与 `Cargo.toml` 版本一致的 `vX.Y.Z` 标签时，工作流验证完整矩阵，然后上传到 GitHub Release，并重新下载核对发布文件与已验收产物逐字节一致。版本标签校验、任意组合构建或安装验证失败都会阻止发布。手动工作流提供可下载的 Actions Artifacts。

包名结构：

```text
geometry_pyramid-v<VERSION>-pg<MAJOR>-<PLATFORM>-<ARCH>.tar.gz
geometry_pyramid-v<VERSION>-pg<MAJOR>-windows-2022-x86_64.zip
```

完整发布包含 20 份安装包、20 份对应的 `.sha256` 文件，以及 `release-manifest.json`、`SHA256SUMS`，共 42 个资产。每份包对应一个同名 `.sha256` 文件。`release-manifest.json` 记录全部产物的文件名、大小、SHA-256、构建环境、PG 主版本和源代码提交。汇总步骤同时检查是否缺包、包名与包内元数据是否一致、全部产物是否来自同一个提交。

## Linux 与 macOS 安装

先在 PostgreSQL 服务器上安装匹配版本的 PostGIS。解压所选安装包后，进入包内目录，明确指定目标 PostgreSQL 的 `pg_config`：

```bash
# Ubuntu PGDG 的 PostgreSQL 17 示例
./install.sh --pg-config /usr/lib/postgresql/17/bin/pg_config --dry-run
sudo ./install.sh --pg-config /usr/lib/postgresql/17/bin/pg_config
```

macOS 可以使用 Homebrew 安装的目标 PostgreSQL：

```bash
./install.sh --pg-config "$(brew --prefix postgresql@17)/bin/pg_config" --dry-run
./install.sh --pg-config "$(brew --prefix postgresql@17)/bin/pg_config"
```

安装所需权限取决于目标 PostgreSQL 目录的所有者。`--dry-run` 验证文件校验值、PG 主版本、系统和架构并显示目标位置；实际安装通过同目录临时文件和重命名替换动态库，使已有后端保留其已加载的旧文件内容。升级后重新建立数据库连接。

制作系统包时，可以使用 `--destdir /absolute/staging/root` 将文件安装到暂存根目录。此选项保留目标 `pg_config` 所定义的目录层级，可用于后续 `.deb`、`.rpm` 封装；对应发行版的系统依赖仍应由该发行版的打包工具生成并验证。

## Windows 安装

先安装对应 PostgreSQL 所需的 Microsoft Visual C++ 运行库（通常由 EDB 安装器安装）。安装与 PostgreSQL 主版本匹配的 PostGIS，并使用 **PowerShell 7 或更新版本**打开解压后的目录：

```powershell
./install.ps1 -PgConfig 'C:\Program Files\PostgreSQL\17\bin\pg_config.exe' -DryRun
./install.ps1 -PgConfig 'C:\Program Files\PostgreSQL\17\bin\pg_config.exe'
```

向 `Program Files` 等受保护目录安装时，以管理员身份运行 PowerShell。升级前停止使用这套 PostgreSQL 安装的服务，完成文件替换后再启动。安装器检查目标 `postgres.exe` 的实际 CPU 架构；旧 DLL 被进程占用时会停止安装并说明处理方式。

## 在业务数据库中启用

文件安装到 PG 服务器后，使用数据库管理员连接需要启用扩展的业务库：

```sql
SELECT name, default_version, installed_version
FROM pg_available_extensions
WHERE name = 'geometry_pyramid';

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION geometry_pyramid;

SELECT geometry_pyramid_version();
```

每个业务数据库分别执行 `CREATE EXTENSION`。安装脚本只管理扩展程序文件，数据库启用和升级由管理员选择目标库执行。

已有 `0.1.0` 数据库升级时，按照 [README 中的升级步骤](../README.zh-CN.md) 在同一个事务中执行 `ALTER EXTENSION` 并绑定原有源表与瓦片表；安装新文件本身不会更新数据库内的扩展版本。

在 PG15–18 上，已有 `0.1.1`、`0.1.2` 数据库无需重新绑定或重建瓦片。停止正在执行的扩展任务，安装 `0.1.3` 文件并重新建立数据库连接后执行：

```sql
ALTER EXTENSION geometry_pyramid UPDATE TO '0.1.3';
SELECT geometry_pyramid_version();
```

## 从源码生成单个安装包

准备 Rust stable、与项目依赖一致的 `cargo-pgrx`、目标 PG 的开发文件以及 PostGIS。在与目标相匹配的系统和架构上执行：

```bash
cargo install --locked cargo-pgrx --version 0.18.0
cargo pgrx init --pg17 /usr/lib/postgresql/17/bin/pg_config

cargo pgrx package \
  --pg-config /usr/lib/postgresql/17/bin/pg_config \
  --no-default-features --features pg17 \
  --out-dir package

python3 scripts/package.py \
  --staging package \
  --pg-config /usr/lib/postgresql/17/bin/pg_config \
  --platform ubuntu-24.04 --arch x86_64 \
  --out-dir dist
```

`cargo pgrx package` 默认使用 release profile。`scripts/package.py` 将 pgrx 的安装暂存树整理为可迁移目录，读取 ELF/Mach-O/PE 文件头核实实际系统与架构，并生成校验文件。构建需要 Python 3.12；用户安装只需要包内的 Bash 或 PowerShell 脚本。

Windows 使用同样的 `cargo pgrx package` 与 Python 打包命令，将 `pg_config` 替换为真实 `.exe` 路径，平台参数设为 `windows-2022`，架构设为 `x86_64`。

## 安装包验收

每个原生构建作业按以下顺序验收：

1. 准备匹配的 PostgreSQL 和可生成 MVT 的 PostGIS。
2. 运行项目 Rust/PostGIS 回归测试。
3. 构建候选版本与固定提交的真实 `v0.1.0`、`v0.1.2` 基线安装文件。
4. 生成最终安装压缩包，通过包内安装器完成安装。
5. 在相互独立的临时集群验证 `0.1.0` 和 `0.1.2` 升级、原有瓦片与授权保留；`0.1.2` 升级不补写关系绑定。
6. 在空数据库验证首次启用和版本一致性，运行跨会话权限、事务与瓦片检查。
7. 所有目标通过后汇总产物并校验完整性，再进入标签发布步骤；发布后重新下载，验证实际发布文件与验收产物相同。

本地可以运行打包工具的回归检查：

```bash
python3 -m unittest discover -s tests -p 'test_packaging_*.py' -v
```

实际安装烟测会替换所选 PG 安装目录中的扩展文件，应在 CI 或专门的临时 PostgreSQL 安装中运行：

```bash
python3 tests/package_smoke.py \
  --archive /path/to/candidate.tar.gz \
  --legacy-staging /path/to/legacy-package \
  --previous-staging /path/to/previous-package \
  --pg-config /path/to/disposable-postgresql/bin/pg_config
```

## 构建依赖来源

- Ubuntu：PGDG 提供与主版本对应的 PostgreSQL、开发文件和 PostGIS 包。
- macOS：pgrx 构建目标 PG，PostGIS 3.6.4 源码使用固定 SHA-256 校验后针对同一个 `pg_config` 编译；Homebrew 提供 GEOS、PROJ、protobuf-c 等构建依赖。
- Windows：pgrx 下载 EDB PostgreSQL；OSGeo 对应主版本的固定 PostGIS ZIP 经过供应方校验值验证，并在构建日志中记录 SHA-256。

维护入口：

- [发布工作流](../.github/workflows/release.yml)
- [构建环境准备](../scripts/ci/setup-postgres.sh)
- [Windows 构建环境准备](../scripts/ci/setup-postgres.ps1)
- [安装包生成器](../scripts/package.py)
- [PostgreSQL 扩展文件规范](https://www.postgresql.org/docs/17/extend-extensions.html)
- [pgrx 0.18.0 构建说明](https://github.com/pgcentralfoundation/pgrx/blob/v0.18.0/cargo-pgrx/README.md)
