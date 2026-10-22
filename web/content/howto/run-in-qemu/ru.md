---
title: "Запуск nos в QEMU"
description: "Загрузка nos на x86-64 с ISO или образа диска, под KVM на Linux и на arm64 с HVF на Apple Silicon, и как отличить удачную загрузку от неудачной."
order: 2
updated: 2026-10-22
---

В итоге nos будет работать в QEMU: перед вами будет его шелл, а лог последовательного порта — в файле. Вы также узнаете, какие строки лога означают удачную загрузку и какие параметры ядра менять, если она не удалась.

Понадобится:

- файлы из руководства [«Сборка nos»](/howto/build): `nos.iso` для загрузки x86-64 с ISO, `nos.qcow2` — с образа диска, `nos-arm64.img` — для arm64;
- QEMU на хосте: `qemu-system-x86_64`, `qemu-system-aarch64` и `qemu-img`;
- Linux с KVM для `scripts/run.sh` и Mac на Apple Silicon для HVF. Всё остальное работает под TCG где угодно, только медленнее;
- свободные порты хоста 2222 и 9000: скрипты пробрасывают их в гостевую систему.

## Скрипты

| Скрипт | Что загружает | Машина | Консоль |
|---|---|---|---|
| `scripts/qemu.sh` | `nos.iso` | 2 CPU, virtio-net | окно QEMU; последовательный порт — в `nos.log` |
| `scripts/qemu-disk.sh` | `nos.qcow2` | 4 CPU, 8 ГБ, virtio-blk, virtio-scsi с тремя дисками, NVMe, virtio-net, virtio-rng | последовательный порт в вашем терминале |
| `scripts/run.sh` | `nos.iso` под KVM | все CPU хоста, virtio-scsi, `isa-debug-exit` | VNC-дисплей `:0`; последовательный порт — в `nos.log` |
| `scripts/qemu-arm64.sh` | `nos-arm64.img` на `virt` | 4 CPU, 1 ГБ, GICv3, virtio-mmio blk, net и rng | последовательный порт — в `nos-arm64.log` |

`qemu.sh`, `qemu-disk.sh` и `qemu-arm64.sh` пробрасывают TCP-порт 2222 хоста на порт 22 гостя — для [SSH](/howto/connect). `qemu-disk.sh` и `qemu-arm64.sh` дополнительно пробрасывают UDP-порт 9000 — для UDP-шелла. `qemu.sh`, `qemu-disk.sh` и `run.sh` запускают GDB-заглушку QEMU на порту 1234 (`-s`). `qemu-arm64.sh` передаёт QEMU все дополнительные аргументы, так что там то же самое даёт `./scripts/qemu-arm64.sh -s`.

## Загрузка x86-64 с ISO

```sh
./scripts/qemu.sh
```

GRUB загружает ядро с командной строкой из [`build/grub.cfg`](https://github.com/irqlevel/nos/blob/master/build/grub.cfg): `dhcp=auto dns=on root=auto fstest=on`. Диск не подключён, поэтому `/` — это ramfs, и ничего из записанного не переживёт перезагрузку. `fstest=on` прогоняет на ней самотест файловой системы. Скрипт не включает KVM, поэтому QEMU эмулирует процессор через TCG, и загрузка идёт медленно: smoke-тест даёт ей до 300 секунд. Для KVM на Linux используйте `run.sh` (см. ниже).

Чтобы работать в шелле, щёлкните в окно QEMU и печатайте. Всё, что выводит шелл, уходит и в последовательный порт, который этот скрипт пишет в `nos.log` вместе с логом загрузки. Следить за логом из другого терминала:

```sh
tail -f nos.log
```

Чтобы файлы сохранялись между перезагрузками, дайте загрузке с ISO корневую файловую систему: образ ext2 с меткой `nos`, который `root=auto` находит и монтирует на чтение и запись (см. [Making a root filesystem](/docs/filesystems#making-a-root-filesystem)). Ниже — та же машина, что в `qemu.sh`, с добавленным диском:

```sh
./scripts/mkrootfs.sh root.img 256
qemu-system-x86_64 -smp 2 -cdrom nos.iso -serial file:nos.log -vga std \
    -drive file=root.img,format=raw,if=virtio \
    -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
    -netdev user,id=net0,hostfwd=tcp::2222-:22
```

`mkrootfs.sh` вызывает `mkfs.ext2` напрямую, если установлен e2fsprogs, а иначе — внутри образа `nos-builder`.

### Загрузка через UEFI

Тот же ISO загружается под OVMF. Тогда GRUB передаёт ядру пиксельный фреймбуфер, и nos рисует на нём консоль шрифтом 8x16. Поправьте пути к прошивке на те, где OVMF лежит у вас:

```sh
cp /usr/share/OVMF/OVMF_VARS_4M.fd /tmp/ovmf_vars.fd
qemu-system-x86_64 -smp 2 -m 1G -cdrom nos.iso -serial file:nos.log \
    -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
    -drive if=pflash,format=raw,unit=1,file=/tmp/ovmf_vars.fd
```

## Загрузка x86-64 с образа диска

```sh
./scripts/build-disk.sh
./scripts/qemu-disk.sh
```

`qemu-disk.sh` загружает `nos.qcow2` с virtio-blk и подключает остальные устройства, которые поддерживают драйверы: legacy-контроллер virtio-scsi с тремя дисками, диск NVMe, virtio-net и virtio-rng. Гость получает 4 CPU и 8 ГБ памяти. При первом запуске скрипт создаёт в корне репозитория рабочие диски `scsi-disk0.qcow2`, `scsi-disk1.qcow2`, `scsi-disk2.qcow2` и `nvme-disk0.qcow2`, а следующие запуски используют их же.

Пункт GRUB на диске ([`build/grub-disk.cfg`](https://github.com/irqlevel/nos/blob/master/build/grub-disk.cfg)) загружает ядро с `console=serial dhcp=auto dns=on root=auto`, а скрипт запускает QEMU с `-nographic`, так что GRUB, а затем и шелл появляются в вашем терминале. Корень — раздел ext2 на этом диске, смонтированный на чтение и запись, поэтому всё, что вы запишете из шелла, переживёт перезагрузку. Чтобы закончить сеанс, выполните в шелле `poweroff` или нажмите управляющую последовательность QEMU: Ctrl-A, затем X.

## Загрузка x86-64 под KVM

На Linux с KVM:

```sh
./scripts/run.sh
```

Скрипт отдаёт гостю все CPU хоста, KVM и небольшой рабочий диск virtio-scsi (`scsi-disk.img`, обнуляется при каждом запуске). Последовательный порт он пишет в `nos.log`, а экран выводит на VNC-дисплей `:0`: чтобы печатать в шелле, подключитесь VNC-клиентом. Скрипт печатает каждую команду, которую выполняет. Устройства virtio-net он не добавляет, поэтому в такой загрузке у nos нет сетевой карты, с которой он умеет работать. Зато он подключает устройство QEMU `isa-debug-exit`, к которому `poweroff` прибегает, если ACPI не выключил машину. Самый короткий эквивалент из раздела [Run](/docs/run):

```sh
qemu-system-x86_64 -enable-kvm -smp 8 -cdrom nos.iso -serial file:nos.log \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04
```

## Загрузка arm64 на QEMU `virt`

```sh
./scripts/qemu-arm64.sh
```

На Mac с Apple Silicon скрипт использует HVF (`-accel hvf -cpu host`) — это примерно вчетверо быстрее TCG. На любой другой машине он использует TCG с Cortex-A72. Чтобы принудительно включить TCG на Mac:

```sh
NOS_TCG=1 ./scripts/qemu-arm64.sh
```

QEMU загружает ядро через `-kernel nos-arm64.img`, командная строка — `dhcp=auto dns=on root=auto`, последовательный порт пишется в `nos-arm64.log`; дисплея нет. Лог можно читать, но печатать в ядро через него нельзя. Чтобы работать в шелле, включите UDP-шелл (`NOS_UDPSHELL=9000 ./scripts/qemu-arm64.sh`) или войдите по SSH — оба способа описаны в руководстве [«Подключение к работающему nos»](/howto/connect). Скрипт запускает QEMU через `exec`, поэтому Ctrl-C в его терминале останавливает машину.

Диск virtio у гостя — `nos-arm64.qcow2`; скрипт создаёт его пустым (256 МБ) при первом запуске. На таком диске нет файловой системы, и `/` будет ramfs. Чтобы корень сохранял записанное, замените диск образом ext2:

```sh
./scripts/mkrootfs.sh root-arm64.img 256
qemu-img convert -f raw -O qcow2 root-arm64.img nos-arm64.qcow2
```

## Как выглядит удачная загрузка

Каждая строка лога загрузки имеет вид `level:seconds.microseconds:function(),file,line: message`. Загрузку делят три маркера, и smoke-тесты ждут их все:

- `After test` — самотесты при загрузке прошли;
- `Preempt is now on` — планировщик работает, остальные CPU запущены;
- `boot: complete` — шелл и сетевые службы работают.

На x86-64 они идут именно в таком порядке. На arm64 порядок другой: сначала `Self test passed` и `Cpus started, N running`, затем `Preempt is now on`, затем `After test` (там он идёт после теста многозадачности) и в конце `boot: complete`. Строки `MountRootFs:` сообщают, что смонтировано как `/`, а загрузка с `fstest=on` выводит `fstest: passed`. `PANIC:` в любом месте лога, включая `RUST PANIC:`, означает, что загрузка не удалась; как читать такой отчёт, описано в руководстве [«Отладка nos с GDB»](/howto/debug-with-gdb).

```sh
grep -e 'After test' -e 'Preempt is now on' -e 'boot: complete' -e 'PANIC:' nos.log
```

После `boot: complete` шелл печатает баннер, получает аренду DHCP (пользовательская сеть QEMU выдаёт `10.0.2.15`) и показывает приглашение:

```text
  _   _  ___  ____
 | \ | |/ _ \/ ___|
 |  \| | | | \___ \
 | |\  | |_| |___) |
 |_| \_|\___/|____/

DHCP auto on eth0...
DHCP ip: 10.0.2.15
DNS resolver started, server: 10.0.2.3
$
```

Чтобы проверить сборку, не наблюдая за загрузкой, запустите её без экрана и посмотрите на код возврата: 0 значит, что появились все маркеры, 1 — что загрузка не удалась, например из-за паники или таймаута. Третья команда запускает тест arm64 под HVF вместо TCG:

```sh
./scripts/smoke-test.sh --skip-build
./scripts/smoke-arm64.sh --skip-build
SMOKE_HVF=1 ./scripts/smoke-arm64.sh --skip-build
```

Smoke-скрипты сами создают рабочие диски и пишут лог во временный каталог. `SMOKE_TIMEOUT` (по умолчанию 300 секунд) задаёт время ожидания, `SMOKE_LOG` — путь к логу. Без `--skip-build` они сначала собирают ядро в контейнере `nos-builder`.

## Первые команды

| Команда | Что показывает |
|---|---|
| `help` | все команды, включая добавленные модулями |
| `version` | релиз и коммит, из которого собрано ядро |
| `uptime`, `date` | время с момента загрузки; текущие дату и время |
| `lscpu` | процессор и возможности, на которые опирается ядро |
| `ps`, `top` | задачи; загрузку CPU по задачам за интервал |
| `meminfo`, `memusage` | карту памяти от прошивки и то, сколько из неё использует ядро |
| `pci`, `disks`, `mounts`, `ls /` | устройства PCI, блочные устройства, точки монтирования, файлы |
| `net`, `arp`, `nslookup example.com` | сетевые устройства и счётчики, таблицу ARP, DNS-запрос |
| `dmesg 40`, `dmesg 100 virtio` | последние строки лога ядра, с фильтром или без |
| `lsmod` | загруженные модули |
| `profile` | где ядро проводит время — целыми цепочками вызовов |
| `poweroff`, `reboot` | выключение (ACPI S5), перезагрузка |

Пока работает шелл, трассировка в консоль больше не выводится. Она по-прежнему попадает в лог ядра, который читает `dmesg`. Полный список команд — в разделе [Shell commands](/docs/shell-commands).

## Параметры ядра

Командная строка ядра — это пары `key=value`, разделённые пробелами. Где её задавать, зависит от способа загрузки:

| Загрузка | Где командная строка | Что сделать после правки |
|---|---|---|
| x86-64 с ISO (`qemu.sh`, `run.sh`) | строка `multiboot2` в `build/grub.cfg` | пересобрать `nos.iso` (`make nocheck`) |
| x86-64 с диска (`qemu-disk.sh`) | строка `multiboot2` в `build/grub-disk.cfg` | заново запустить `./scripts/build-disk.sh` |
| arm64 (`qemu-arm64.sh`) | `APPEND=` в `scripts/qemu-arm64.sh` | ничего: следующая загрузка прочитает её |

Ничто в командной строке не останавливает загрузку: неизвестный ключ или значение ядро записывает в лог и пропускает. Длина строки ограничена 255 символами. Когда загрузка не удаётся, эти параметры исключают подсистемы по одной:

| Параметр | Действие |
|---|---|
| `smp=off` | работать только на загрузочном CPU |
| `maxcpus=N` | запустить не больше N CPU, включая загрузочный |
| `loglevel=N` | уровень трассировки при загрузке, 0–5 (по умолчанию 1) |
| `usb=off` | x86-64: не инициализировать xHCI |
| `its=off` | arm64: отключить GICv3 ITS; MSI для PCIe деградирует, virtio-mmio это не затрагивает |
| `hwrng=off` | не использовать инструкцию процессора для случайных чисел |
| `rc=off` | не выполнять `/etc/rc` |
| `dhcp=off` | полностью отключить DHCP |
| `console=serial`, `console=vga` | шелл только на одной из консолей |
| `trace=vga` | x86-64: выводить лог загрузки на экран, а не только в последовательный порт |

Пробуйте их в таком порядке: `smp=off`, затем `maxcpus=2`, затем `loglevel=4`, затем подсистемы по одной. Остальная часть метода — в руководстве [«Отладка nos с GDB»](/howto/debug-with-gdb), все параметры — в разделе [Kernel parameters](/docs/kernel-parameters).

## Другие способы запуска

- `scripts/qemu-igb.sh` загружает ISO с сетевой картой QEMU Intel 82576 (`igb`) вместо virtio-net. Это единственный способ запустить драйвер настоящей сетевой карты без реального железа; нужен QEMU 8.0 или новее. `NOS_PCAP=1` дополнительно пишет захват пакетов в `nos-igb.pcap`, а `NOS_TRACE=1` — трассировку модели сетевой карты в `nos-igb-trace.log`.
- `nos.qcow2` загружается в Google Cloud и других KVM-облаках; см. [Run: Google Cloud](/docs/run#google-cloud).

## Решение проблем

- **QEMU сразу завершается с ошибкой о правиле проброса порта.** Порт хоста 2222 или 9000 уже занят — часто другим QEMU, который всё ещё работает.
- **HVF не используется.** Скрипт выбирает HVF только на Mac с Apple Silicon (когда `uname -sm` выводит `Darwin arm64`). В остальных случаях он переходит на TCG.
- **Загрузка идёт медленно.** TCG эмулирует каждую инструкцию. На Mac x86-64 всегда работает под TCG, а arm64 — при `NOS_TCG=1`.
- **Ничего из напечатанного не доходит до ядра arm64.** Его последовательный порт — это файл. Используйте UDP-шелл или SSH.
- **Файлы пропали после перезагрузки.** `/` был ramfs; в логе будет `MountRootFs: mounted ramfs on / (rw)`. Загрузитесь с образа диска или подключите корневой образ, как показано выше.
- **Шелл выводит `DHCP auto timeout`.** nos нашёл сетевую карту, но за десять секунд не получил аренду DHCP: проверьте часть `-netdev` командной строки QEMU. Если сетевой карты, с которой nos умеет работать, нет вовсе, шелл не выводит ни одной строки про DHCP.

## Что дальше

- [Подключение к работающему nos](/howto/connect): UDP-шелл, SSH и netconsole
- [Отладка nos с GDB](/howto/debug-with-gdb)
- [Запуск гейтов](/howto/run-the-gates)
- [Linux-гость под гипервизором nos](/howto/linux-guest)
- [Boot](/docs/boot): что происходит между GRUB и `boot: complete`
