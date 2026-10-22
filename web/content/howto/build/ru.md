---
title: "Сборка nos"
description: "Как собрать ядро, модули и загрузочные образы для x86-64 и arm64: в Docker на macOS или нативно на Linux с закреплённым nightly Rust."
order: 1
updated: 2026-10-22
---

В итоге у вас будет nos, собранный из исходников: ISO и ядро для x86-64, образ ядра для arm64, загружаемые модули и, если нужно, образ диска для QEMU и KVM-облаков.

Понадобится:

- клон репозитория;
- либо Docker — так nos собирают на macOS, в том числе на Apple Silicon, — либо Linux с установленным тулчейном. Эталонная система — Ubuntu 22.04: на ней основан образ сборщика и на ней же работает CI;
- место на диске: образ сборщика занимает около 4,7 ГБ, результаты сборки обеих архитектур — ещё пару гигабайт в `out/` и `src/rust/target/`;
- доступ в сеть, чтобы установить тулчейн. Сама сборка в сеть не ходит.

```sh
git clone https://github.com/irqlevel/nos.git
cd nos
```

## Сборка в Docker

```sh
./scripts/build-iso-docker.sh
```

Скрипт собирает образ `nos-builder` по [Dockerfile](https://github.com/irqlevel/nos/blob/master/Dockerfile). Это Ubuntu 22.04 для `linux/amd64` с clang, lld, nasm, GRUB для BIOS и UEFI, xorriso, mtools, cppcheck, e2fsprogs, squashfs-tools, QEMU и закреплённым nightly Rust. Затем скрипт запускает `make clean && make all` в контейнере, куда ваш рабочий каталог смонтирован как `/src`. В образе только тулчейн, а исходники монтируются при запуске контейнера, поэтому после их правки пересобирать образ не нужно. По окончании сборки скрипт выводит:

```text
Built nos.iso and kernel64.elf (use kernel64.elf in GDB for symbols)
```

`make all` сначала прогоняет cppcheck, затем собирает ISO для x86-64 и все модули. Скрипт начинает с `make clean`, так что каждый запуск — полная пересборка. На Mac с Apple Silicon образ `linux/amd64` работает под эмуляцией, поэтому сборка занимает заметное время.

### Пересборка только изменённого

Когда образ уже есть, `make` можно запускать в нём напрямую — так же собирает smoke-тест:

```sh
docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder \
    bash -c 'make rust && make nocheck'
```

Для arm64:

```sh
docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder \
    bash -c 'make nocheck ARCH=aarch64'
```

> В образе нет git. Скрипты сборки вычисляют имя релиза и коммит на хосте и передают их в контейнер как `VERSION` и `GIT_DESC`. Если запустить `docker run` самостоятельно без них, команда ядра `version` выведет `nos dev (unknown)`. Чтобы сохранить коммит, добавьте `-e GIT_DESC="$(git rev-parse --short HEAD)"`, как делает релизный workflow.

### Сборка образа диска

```sh
./scripts/build-disk.sh
```

Скрипт собирает ядро в контейнере (`make clean && make nocheck`) и затем записывает `nos.qcow2`. Это MBR-диск с GRUB в загрузочном секторе и одним разделом ext2 с меткой `nos`. На этом разделе лежит `/boot/kernel64.elf`, и nos монтирует его на чтение и запись как корневую файловую систему. `--privileged` скрипту не нужен. Результат меняют две переменные:

```sh
SIZE_MB=4096 ROOTFS=my-root ./scripts/build-disk.sh
```

`SIZE_MB` задаёт размер в МБ (по умолчанию 1024). `ROOTFS` указывает каталог, который копируется в корневую ФС рядом с `/boot`: например, модули в `lib/modules`, `/etc/rc` или SSH-ключи. Диск загружается только через BIOS — так его загружают KVM-облака (см. [Run: Google Cloud](/docs/run#google-cloud)). Скрипт собирает и ядро, поэтому тот же запуск даёт ещё `nos.iso` и `bin/kernel64.elf`.

### Сборка системного образа

```sh
./scripts/build-image.sh
./scripts/build-image.sh --clean
```

Обе команды создают `nos-x86_64.squashfs` — ядро, все модули и пункты меню GRUB, из которых разворачивают сервер (см. [Установка nos на выделенный сервер](/howto/install-on-a-server)). Без аргумента скрипт пересобирает то, что изменилось с прошлой сборки. С `--clean` он собирает всё с нуля, как при выпуске релиза.

## Нативная сборка на Linux

На Ubuntu 22.04 установите те же пакеты, что есть в образе сборщика:

```sh
sudo apt-get update
sudo apt-get install -y build-essential clang lld llvm nasm \
    grub2-common grub-pc-bin grub-efi-amd64-bin xorriso mtools cppcheck \
    parted e2fsprogs squashfs-tools qemu-utils qemu-system-x86 qemu-system-arm \
    python3 curl
```

Тулчейн Rust должен быть датированным nightly с компонентом `rust-src`. Выполните команду ниже из корня репозитория. Её же использует CI, и она устанавливает ровно тот nightly, что указан в [`src/rust/rust-toolchain.toml`](https://github.com/irqlevel/nos/blob/master/src/rust/rust-toolchain.toml):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --component rust-src --default-toolchain \
    "$(sed -n 's/^channel *= *"\(.*\)"/\1/p' src/rust/rust-toolchain.toml)"
export PATH="$HOME/.cargo/bin:$PATH"
```

Затем соберите:

```sh
make
```

| Команда | Что делает |
|---|---|
| `make` | запускает `make check`, затем собирает `nos.iso` и модули |
| `make nocheck` | то же без cppcheck |
| `make check` | только cppcheck по `src/cpp`; любая находка — ошибка |
| `make rust` | только Rust-staticlib `libkernel.a` |
| `make modules` | только модули |
| `make nocheck ARCH=aarch64` | ядро arm64, его образ и модули |
| `make image` | `nos-x86_64.squashfs` (только x86-64, нужен `mksquashfs`) |
| `make nocheck UBSAN=1` | C++ под UB-санитайзером clang (см. ниже) |
| `make nocheck RUSTUB=1` | Rust с проверками предусловий unsafe-операций `core` и переполнений (см. ниже) |
| `make smoke` | запускает `scripts/smoke-test.sh`: сборка в Docker и загрузка без экрана |
| `make clean` | удаляет `out/`, ядра, `nos-arm64.img`, ISO и системный образ и выполняет `cargo clean` |

`ARCH` выбирает архитектуру: `x86_64` (по умолчанию) или `aarch64`. Объектные файлы попадают в `out/$(ARCH)/`. Для сборки только под arm64 достаточно make, clang, lld, llvm и nightly — больше релизный workflow для неё ничего не ставит. `make smoke` собирает в Docker. С нативным тулчейном соберите через `make nocheck` и затем запустите `./scripts/smoke-test.sh --skip-build`.

## Что получится

| Файл | Что это |
|---|---|
| `nos.iso` | загрузочный ISO для x86-64 с GRUB и ядром; грузится и через BIOS, и через UEFI |
| `kernel64.elf`, `bin/kernel64.elf` | ядро x86-64; скрипты GDB загружают копию из `bin/` |
| `nos-arm64.img` | ядро arm64 в формате Linux `Image` для QEMU `-kernel` |
| `kernel-arm64.elf`, `bin/kernel-arm64.elf` | ELF ядра arm64 с символами |
| `out/<arch>/modules/*.ko` | модули: `hello`, `modtest`, `slowexit`, `blkload`, `netblk`, `netload`, `sshd`, `hv`, `heapbench` |
| `nos.qcow2` | образ диска из `scripts/build-disk.sh` |
| `nos-x86_64.squashfs` | системный образ из `make image` или `scripts/build-image.sh` |

Модуль загружается только в ядро, собранное из того же дерева. Загрузчик сверяет дайджест интерфейса ядра и отвергает модуль, собранный под другой (см. [What a module may call](/docs/modules#what-a-module-may-call)). Релизы по тегам содержат ядра, `nos.iso`, `nos.qcow2`, `nos-arm64.img`, системный образ и все модули в готовом виде ([`release.yml`](https://github.com/irqlevel/nos/blob/master/.github/workflows/release.yml)).

## Как закреплён тулчейн и вендорятся зависимости

- `src/rust/rust-toolchain.toml` закрепляет датированный nightly (на момент написания — `nightly-2026-08-30`) с `rust-src`. Rust-коду ядра nightly нужен по двум причинам: `-Z build-std`, который пересобирает `core` и `alloc` с большой моделью кода, и фича `alloc_error_handler`.
- Внешние крейты лежат в `src/rust/vendor`, а Makefile запускает cargo с `--offline`. Среди них rustls и крейты RustCrypto, на которых построены TLS и SSH, а также крейты, от которых зависит сама std. Ни одна сборка не обращается к crates.io. Если сборка просит сеть, значит, какую-то зависимость забыли завендорить, и cargo завершится ошибкой вместо того, чтобы её скачать.
- После того как вы добавили, удалили или обновили зависимость в `src/rust`, запустите `scripts/vendor.sh` — единственный шаг, которому нужна сеть. Закоммитьте обновлённый `src/rust/vendor` вместе с `Cargo.lock`. Если вы обновляете закреплённый nightly, перезапустите `scripts/vendor.sh` в том же коммите: в вендоренный набор входят зависимости самой std, а они меняются от одного nightly к другому.
- Dockerfile и оба workflow CI берут версию из этого единственного файла, больше она нигде не записана.

## Сборка с проверками неопределённого поведения

```sh
make nocheck UBSAN=1
make nocheck RUSTUB=1
make nocheck ARCH=aarch64 UBSAN=1 RUSTUB=1
```

`UBSAN=1` собирает C++ с UB-санитайзером clang. `RUSTUB=1` включает проверки предусловий unsafe-операций самого `core` и проверки арифметических переполнений в Rust. Флаги можно использовать по отдельности или вместе. Первая же сработавшая проверка вызывает панику с указанием места:

```text
PANIC:Report():ubsan.cpp,267: UBSAN: shift exponent 64 is too large for 64-bit type 'unsigned long' at src/cpp/...
```

Выходные файлы называются как обычно. Инструментированные объекты C++ попадают в `out/<arch>-ubsan/`, а метка в `out/` перелинковывает образы при каждой смене варианта сборки, так что обычная сборка никогда не выпустит инструментированное ядро. Команда `version` добавляет к выводу `+ubsan` или `+rustub`. Чтобы за одну загрузку собрать все отчёты C++, а не паниковать на первом, загрузите ядро с `ubsan=warn`. Подробнее — в разделах [UBSan](/docs/build#ubsan) и [Rust UB checks](/docs/build#rust-ub-checks).

## Обе архитектуры должны оставаться зелёными

Для `x86_64` и для `aarch64` CI ([`ci.yml`](https://github.com/irqlevel/nos/blob/master/.github/workflows/ci.yml)) выполняет `make check`, `make nocheck ARCH=...` и smoke-загрузку под TCG. Последним шагом он собирает ядро с `UBSAN=1 RUSTUB=1` и загружает эту сборку тоже. Изменение в общем коде, которое собирается только на x86, ломает CI.

За двумя вещами нужно следить:

- **Списки исходников явные.** Makefile перечисляет каждый файл C++ в `CXX_SRC_x86_64` и `CXX_SRC_aarch64`, а ассемблерные — в `ASM_SRC_*` и `ASM_S_SRC_*`. Новый `.cpp`, которого нет в списке, молча не компилируется, поэтому переносимый код добавляйте в оба списка. Если общий код вызывает точку входа, существующую только на x86, добавьте недостижимую заглушку для линковки в [`x86_driver_stubs.cpp`](https://github.com/irqlevel/nos/blob/master/src/cpp/arch/arm64/x86_driver_stubs.cpp), а не `#ifdef`.
- **Результат прогона определяет код возврата.** Прежде чем отправить изменение, соберите и загрузите обе архитектуры:

```sh
make nocheck && ./scripts/smoke-test.sh --skip-build
make nocheck ARCH=aarch64 && ./scripts/smoke-arm64.sh --skip-build
```

На macOS запускайте `./scripts/smoke-test.sh` и `./scripts/smoke-arm64.sh` без `--skip-build`: каждый скрипт сначала сам инкрементально соберёт ядро в контейнере `nos-builder`. Smoke-скрипт завершается с кодом 0, если загрузка вывела все маркеры, и с кодом 1 при панике, таймауте или неудачной сборке. Что покрывает каждая проверка, описано в руководстве [«Запуск гейтов»](/howto/run-the-gates).

## Решение проблем

- **`No rule to make target` после переноса, переименования или удаления заголовка.** Устаревший файл зависимостей (`.d`) в `out/` всё ещё ссылается на старый путь. Выполните `make clean` или удалите `.d`-файлы, где упомянут старый путь. В сообщении нет слова «error», поэтому проверяйте код возврата `make`, а не ищите ошибки в выводе. И не пропускайте вывод сборки через `tail`: тогда конвейер вернёт код возврата `tail`.
- **cppcheck роняет `make` или `build-iso-docker.sh`.** `make check` останавливается на любой находке. Пока вы итерируете, пропускайте его через `make nocheck`, но в CI `make check` выполняется.
- **ISO грузится через BIOS, но не через UEFI.** `grub-mkrescue` не нашёл модули `x86_64-efi` (`grub-efi-amd64-bin`) или `mtools` и молча собрал ISO только для BIOS. В образе сборщика и в CI оба пакета есть.
- **Сборка пытается выйти в сеть.** Какую-то зависимость не завендорили. Запустите `scripts/vendor.sh`.
- **Контейнер скачивает тулчейн Rust или сообщает, что он не установлен.** Ваш образ `nos-builder` старше текущей версии тулчейна. `build-iso-docker.sh` и `build-disk.sh` пересобирают образ при каждом запуске. `build-image.sh` собирает его, только если образа нет вовсе, а smoke-скрипты (и `mkrootfs.sh`, когда он обращается к Docker) берут тот образ, что уже есть. Пересоберите его:

  ```sh
  docker build --platform linux/amd64 -t nos-builder .
  ```

- **Линковка падает с `a static constructor or destructor: this kernel runs none`.** Ядро не выполняет `.init_array`, поэтому линкер отвергает любую глобальную переменную, тип которой имеет не-`constexpr` конструктор или нетривиальный деструктор. Дайте типу `constexpr`-конструктор или сделайте объект локальной статической переменной функции.
- **Линковка падает, потому что функции лежат не там, где говорит таблица символов.** Трёхпроходная линковка проверяет, что таблица символов для бэктрейсов совпадает с итоговым образом. Подробности — в разделе [Build](/docs/build#four-things-the-makefile-will-not-tell-you).

## Что дальше

- [Запуск nos в QEMU](/howto/run-in-qemu)
- [Запуск гейтов](/howto/run-the-gates)
- [Отладка nos с GDB](/howto/debug-with-gdb)
- [Загружаемый модуль ядра на Rust](/howto/write-a-module)
- Полный справочник: [Build](/docs/build)
