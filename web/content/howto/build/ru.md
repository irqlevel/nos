---
title: "Сборка nos"
description: "Как собрать ядро, модули и загрузочные образы для x86-64 и arm64: в Docker на macOS или прямо на Linux с закреплённым nightly Rust."
order: 1
updated: 2026-10-22
---

В итоге у вас будет nos, собранный из исходников: ISO и ядро для x86-64, образ ядра для arm64 и загружаемые модули. Если нужно, можно собрать и образ диска для QEMU и KVM-облаков.

Понадобится:

- клон репозитория;
- Docker (так nos собирают на macOS, в том числе на Apple Silicon) или Linux с установленным тулчейном. Эталонная система — Ubuntu 22.04: на ней основан образ сборщика и на ней же работает CI;
- место на диске: около 4,7 ГБ под образ сборщика и ещё пара гигабайт под результаты сборки обеих архитектур в `out/` и `src/rust/target/`;
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

`make all` сначала прогоняет cppcheck, статический анализатор C и C++, а затем собирает ISO для x86-64 и все модули. Скрипт начинает с `make clean`, так что каждый запуск пересобирает всё с нуля. На Mac с Apple Silicon образ `linux/amd64` работает под эмуляцией, поэтому сборка идёт небыстро.

### Как пересобрать только изменённое

Когда образ уже есть, `make` можно запускать прямо в нём. Точно так же собирает ядро smoke-тест (быстрая загрузка в QEMU без экрана):

```sh
docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder \
    bash -c 'make rust && make nocheck'
```

Для arm64:

```sh
docker run --platform linux/amd64 --rm -v "$PWD:/src" -w /src nos-builder \
    bash -c 'make nocheck ARCH=aarch64'
```

> В образе нет git. Скрипты сборки узнают имя релиза и коммит на хосте и передают их в контейнер как `VERSION` и `GIT_DESC`. Если запустить `docker run` самостоятельно без них, команда ядра `version` выведет `nos dev (unknown)`. Чтобы в сборку попал коммит, добавьте `-e GIT_DESC="$(git rev-parse --short HEAD)"`, как это делает релизный workflow.

### Сборка образа диска

```sh
./scripts/build-disk.sh
```

Скрипт собирает ядро в контейнере (`make clean && make nocheck`) и затем записывает `nos.qcow2`. Это MBR-диск с GRUB в загрузочном секторе и одним разделом ext2 с меткой `nos`. На этом разделе лежит `/boot/kernel64.elf`, и nos монтирует его на чтение и запись как корневую файловую систему. `--privileged` скрипту не нужен. Результат меняют две переменные:

```sh
SIZE_MB=4096 ROOTFS=my-root ./scripts/build-disk.sh
```

`SIZE_MB` задаёт размер диска в МБ (по умолчанию 1024). `ROOTFS` указывает каталог, который копируется в корневую ФС рядом с `/boot`: например, модули в `lib/modules`, `/etc/rc` или SSH-ключи. Диск загружается только через BIOS, так же как его загружают KVM-облака (см. [Run: Google Cloud](/docs/run#google-cloud)). Скрипт собирает и ядро, поэтому тот же запуск даёт ещё `nos.iso` и `bin/kernel64.elf`.

### Сборка системного образа

```sh
./scripts/build-image.sh
./scripts/build-image.sh --clean
```

Обе команды создают `nos-x86_64.squashfs`. В нём лежат ядро, все модули и пункты меню GRUB, то есть всё, из чего разворачивают сервер (см. [Установка nos на выделенный сервер](/howto/install-on-a-server)). Без аргумента скрипт пересобирает только то, что изменилось с прошлой сборки. С `--clean` он собирает всё с нуля, как при выпуске релиза.

## Сборка прямо на Linux

На Ubuntu 22.04 установите те же пакеты, что есть в образе сборщика:

```sh
sudo apt-get update
sudo apt-get install -y build-essential clang lld llvm nasm \
    grub2-common grub-pc-bin grub-efi-amd64-bin xorriso mtools cppcheck \
    parted e2fsprogs squashfs-tools qemu-utils qemu-system-x86 qemu-system-arm \
    python3 curl
```

Тулчейн Rust должен быть датированным nightly с компонентом `rust-src`. Выполните команду ниже из корня репозитория. Её же использует CI, и она ставит ровно тот nightly, что указан в [`src/rust/rust-toolchain.toml`](https://github.com/irqlevel/nos/blob/master/src/rust/rust-toolchain.toml):

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

`ARCH` выбирает архитектуру: `x86_64` (по умолчанию) или `aarch64`. Объектные файлы попадают в `out/$(ARCH)/`. Для сборки только под arm64 достаточно make, clang, lld, llvm и nightly: больше релизный workflow для неё ничего не ставит. `make smoke` собирает в Docker. С тулчейном на своей машине соберите ядро через `make nocheck`, а затем запустите `./scripts/smoke-test.sh --skip-build`.

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

Модуль загружается только в ядро, собранное из того же дерева. Загрузчик сверяет дайджест интерфейса ядра и отвергает модуль, собранный под другой интерфейс (см. [What a module may call](/docs/modules#what-a-module-may-call)). В релизах по тегам всё лежит в готовом виде: ядра, `nos.iso`, `nos.qcow2`, `nos-arm64.img`, системный образ и все модули ([`release.yml`](https://github.com/irqlevel/nos/blob/master/.github/workflows/release.yml)).

## Как закреплён тулчейн и как устроен вендоринг зависимостей

- `src/rust/rust-toolchain.toml` закрепляет датированный nightly (на момент написания — `nightly-2026-08-30`) с `rust-src`. Rust-коду ядра nightly нужен по двум причинам: ради `-Z build-std`, который пересобирает `core` и `alloc` с большой моделью кода, и ради фичи `alloc_error_handler`.
- Исходники внешних крейтов хранятся прямо в репозитории, в `src/rust/vendor` (это и называют вендорингом), а Makefile запускает cargo с `--offline`. Среди них rustls и крейты RustCrypto, на которых построены TLS и SSH, а также крейты, от которых зависит сама std. Ни одна сборка не обращается к crates.io. Если сборка просит сеть, значит, какую-то зависимость забыли завендорить, и cargo завершится ошибкой, а не скачает её.
- После того как вы добавили, удалили или обновили зависимость в `src/rust`, запустите `scripts/vendor.sh`. Это единственный шаг, которому нужна сеть. Закоммитьте обновлённый `src/rust/vendor` вместе с `Cargo.lock`. Если вы обновляете закреплённый nightly, перезапустите `scripts/vendor.sh` в том же коммите: в вендоренный набор входят зависимости самой std, а они меняются от одного nightly к другому.
- Dockerfile и оба workflow CI берут версию из этого единственного файла, больше она нигде не записана.

## Сборка с проверками неопределённого поведения

```sh
make nocheck UBSAN=1
make nocheck RUSTUB=1
make nocheck ARCH=aarch64 UBSAN=1 RUSTUB=1
```

`UBSAN=1` собирает C++ с UB-санитайзером clang. `RUSTUB=1` включает в Rust два вида проверок: проверки предусловий unsafe-операций самого `core` и проверки арифметических переполнений. Флаги можно использовать по отдельности или вместе. Первая же сработавшая проверка вызывает панику и называет место:

```text
PANIC:Report():ubsan.cpp,267: UBSAN: shift exponent 64 is too large for 64-bit type 'unsigned long' at src/cpp/...
```

Выходные файлы называются как обычно. Инструментированные объекты C++ попадают в `out/<arch>-ubsan/`. Файл-метка в `out/` заставляет сборку перелинковать образы при каждой смене варианта, так что обычная сборка никогда не выпустит инструментированное ядро. Команда `version` добавляет к выводу `+ubsan` или `+rustub`. Чтобы за одну загрузку собрать все отчёты C++, а не паниковать на первом, загрузите ядро с `ubsan=warn`. Подробнее — в разделах [UBSan](/docs/build#ubsan) и [Rust UB checks](/docs/build#rust-ub-checks).

## Обе архитектуры должны оставаться зелёными

Для `x86_64` и для `aarch64` CI ([`ci.yml`](https://github.com/irqlevel/nos/blob/master/.github/workflows/ci.yml)) выполняет `make check`, `make nocheck ARCH=...` и smoke-загрузку под TCG, программным эмулятором процессора в QEMU. Последним шагом CI собирает ядро с `UBSAN=1 RUSTUB=1` и загружает эту сборку тоже. Поэтому изменение в общем коде, которое собирается только на x86, ломает CI.

Следить нужно за двумя вещами:

- **Списки исходников явные.** Makefile перечисляет каждый файл C++ в `CXX_SRC_x86_64` и `CXX_SRC_aarch64`, а ассемблерные файлы — в `ASM_SRC_*` и `ASM_S_SRC_*`. Новый `.cpp`, которого нет в списке, молча не компилируется, поэтому переносимый код добавляйте в оба списка. Если общий код вызывает точку входа, которая есть только на x86, добавьте в [`x86_driver_stubs.cpp`](https://github.com/irqlevel/nos/blob/master/src/cpp/arch/arm64/x86_driver_stubs.cpp) недостижимую заглушку для линковки, а не `#ifdef`.
- **Об успехе прогона судите по коду возврата.** Прежде чем отправить изменение, соберите и загрузите обе архитектуры:

```sh
make nocheck && ./scripts/smoke-test.sh --skip-build
make nocheck ARCH=aarch64 && ./scripts/smoke-arm64.sh --skip-build
```

На macOS запускайте `./scripts/smoke-test.sh` и `./scripts/smoke-arm64.sh` без `--skip-build`: тогда каждый скрипт сначала сам инкрементально соберёт ядро в контейнере `nos-builder`. Smoke-скрипт завершается с кодом 0, если загрузка вывела все маркеры, и с кодом 1 при панике, таймауте или неудачной сборке. Что покрывает каждая проверка, описано в руководстве [«Запуск гейтов»](/howto/run-the-gates).

## Решение проблем

- **`No rule to make target` после переноса, переименования или удаления заголовка.** Устаревший файл зависимостей (`.d`) в `out/` всё ещё ссылается на старый путь. Выполните `make clean` или удалите `.d`-файлы, где упомянут старый путь. В сообщении нет слова «error», поэтому проверяйте код возврата `make`, а не ищите ошибки в выводе. И не пропускайте вывод сборки через `tail`: тогда конвейер вернёт код возврата `tail`.
- **cppcheck роняет `make` или `build-iso-docker.sh`.** `make check` останавливается на любой находке. Пока вы работаете над изменением, можно пропускать проверку через `make nocheck`, но в CI `make check` выполняется.
- **ISO грузится через BIOS, но не через UEFI.** `grub-mkrescue` не нашёл модули `x86_64-efi` (`grub-efi-amd64-bin`) или `mtools` и молча собрал ISO только для BIOS. В образе сборщика и в CI оба пакета есть.
- **Сборка пытается выйти в сеть.** Какую-то зависимость не завендорили. Запустите `scripts/vendor.sh`.
- **Контейнер скачивает тулчейн Rust или сообщает, что он не установлен.** Ваш образ `nos-builder` старше текущей версии тулчейна. `build-iso-docker.sh` и `build-disk.sh` пересобирают образ при каждом запуске. `build-image.sh` собирает его, только если образа нет вовсе, а smoke-скрипты (и `mkrootfs.sh`, когда он обращается к Docker) берут тот образ, что уже есть. Пересоберите его:

  ```sh
  docker build --platform linux/amd64 -t nos-builder .
  ```

- **Линковка падает с `a static constructor or destructor: this kernel runs none`.** Ядро не выполняет `.init_array`, поэтому линкер отвергает любую глобальную переменную, у типа которой не-`constexpr` конструктор или нетривиальный деструктор. Дайте типу `constexpr`-конструктор или сделайте объект статической переменной внутри функции.
- **Линковка падает, потому что функции лежат не там, где говорит таблица символов.** Трёхпроходная линковка проверяет, что таблица символов для бэктрейсов совпадает с итоговым образом. Подробности — в разделе [Build](/docs/build#four-things-the-makefile-will-not-tell-you).

## Что дальше

- [Запуск nos в QEMU](/howto/run-in-qemu)
- [Запуск гейтов](/howto/run-the-gates)
- [Отладка nos с GDB](/howto/debug-with-gdb)
- [Загружаемый модуль ядра на Rust](/howto/write-a-module)
- Полный справочник: [Build](/docs/build)
