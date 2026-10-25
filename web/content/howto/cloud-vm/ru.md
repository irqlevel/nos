---
title: "ВМ из облачного образа"
description: "Дайте nos ссылку на облачный образ дистрибутива и получите ВМ, которая загружает собственное ядро, настраивается через cloud-init с вашим SSH-ключом и снова запускается после перезагрузки nos."
order: 7
updated: 2026-10-25
---

`hv create <name> <url>` делает ВМ из облачного образа дистрибутива так же, как это делает облако, и ничего не нужно делать на другой машине. nos скачивает образ по HTTPS, конвертирует его из qcow2 в сырой диск и увеличивает диск. Он пишет сид для cloud-init с вашим SSH-ключом, а затем `vm.toml`, который загружает собственное ядро образа. ВМ живёт в папке на диске nos, поэтому после перезагрузки nos она никуда не девается. К концу руководства под nos будет работать Debian 13, сам nos — в QEMU, а вы войдёте в гостя по SSH со своей машины.

Что понадобится:

- **x86-64 и расширение виртуализации процессора, которое видит nos.** В QEMU это TCG с `-cpu max` на любом хосте, включая Mac, или KVM с вложенной виртуализацией на Linux-хосте. Все команды ниже выполнялись под TCG на Mac. Под KVM nos пользуется AMD-V или Intel VT-x самого хоста. Под VT-x тот способ, которым загружаются такие ВМ (PVH, о нём ниже), ещё не пробовали.
- **nos, собранный из master** ([Сборка nos](/howto/build)). Релизы до v0.0.25 включительно вышли раньше, чем появился `hv create <url>`. Из одной сборки понадобятся `nos.iso`, `out/x86_64/modules/hv.ko`, `out/x86_64/modules/sshd.ko` и `out/x86_64/hvboot`.
- **Ключ Ed25519** (`ssh-keygen -t ed25519`). sshd в nos не принимает ключей другого типа, а гость получит тот же ключ.
- **Доступ nos в интернет.** Пользовательская сеть QEMU даёт ему NAT и DNS-сервер, а ISO включает DNS (`dns=on`).

Команды рассчитаны на рабочий каталог, в котором лежит клон nos (`nos/`). Как всё устроено внутри, рассказано в посте [ВМ по ссылке](/blog/vms-from-cloud-images).

## 1. Подготовьте корневую файловую систему nos

Положите модули, hvboot и свой ключ туда, где их держит системный образ, и пусть `/etc/rc` запускает sshd при загрузке:

```sh
mkdir -p rootdir/lib/modules rootdir/etc/ssh
cp nos/out/x86_64/modules/hv.ko nos/out/x86_64/modules/sshd.ko rootdir/lib/modules/
cp nos/out/x86_64/hvboot rootdir/lib/hvboot
cp ~/.ssh/id_ed25519.pub rootdir/etc/ssh/authorized_keys
printf 'insmod /lib/modules/sshd.ko\nsshd start\n' > rootdir/etc/rc
nos/scripts/mkrootfs.sh root.img 8192 rootdir
```

- **`/lib/hvboot`** — это ядро, которое `hv create` записывает в `vm.toml` ВМ. hvboot — маленькая программа, которая работает внутри гостя и загружает то ядро, которое загрузил бы собственный GRUB гостя ([A guest's own kernel](/docs/hypervisor#a-guests-own-kernel-pvh-and-hvboot)).
- **`/etc/ssh/authorized_keys`** работает дважды. sshd в nos пускает эти ключи, а `hv create` передаёт их пользователю гостя по умолчанию через cloud-init.
- **Размер.** Пока работает `hv create`, на корне одновременно лежат загрузка и сконвертированный диск. Образ Debian занимает 326 МиБ, а его диск — 907 МиБ, так что 8 ГиБ оставляют гостю место, чтобы расти. `mkrootfs.sh` делает разреженный файл, который занимает только то, что в него записано. На Mac скрипт запускает `mkfs.ext2` в контейнере `nos-builder`, который создала сборка.

## 2. Загрузите nos

```sh
qemu-system-x86_64 -nographic -m 3G -smp 4 -cpu max \
    -cdrom nos/nos.iso \
    -drive file=root.img,format=raw,id=drive0,if=none \
    -device virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off \
    -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
    -netdev user,id=net0,hostfwd=tcp::2222-:22,hostfwd=tcp::2201-:2201
```

На Linux-хосте с KVM и вложенной виртуализацией замените `-cpu max` на `-enable-kvm -cpu host`.

- **`-m 3G`** оставляет место для 1 ГиБ гостя. Если даёте гостю больше через `mem=`, дайте больше и nos.
- **Порт 2222** на вашей машине ведёт к sshd в nos. **Порт 2201** ведёт к порту 2201 в nos, а его `forward=2201:22` ниже пробрасывает собственному sshd гостя.
- **`-nographic`** выводит консоль nos в ваш терминал. `Ctrl-A X` завершает QEMU.

Когда консоль напишет `sshd: listening on port 22`, nos готов. Команды ниже можно набирать в его консоли или по SSH из другого терминала: `ssh -p 2222 root@127.0.0.1`.

## 3. Создайте ВМ

Загрузите гипервизор, включите его и создайте ВМ:

```sh
insmod /lib/modules/hv.ko
hv on
hv create debian https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2 size=8G password=<пароль> forward=2201:22
```

Под TCG на Mac это заняло 84 с, из них 21 с на скачивание:

```text
hv: https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2 -- into /vms/debian/debian-13-genericcloud-amd64.qcow2
download: 5 MiB of 326 MiB, 1%, 2.6 MiB/s
...
download: 342294528 bytes in 21.3 s, 15.2 MiB/s
hv: /vms/debian/debian-13-genericcloud-amd64.qcow2: qcow2 version 3, a disk of 3072 MiB in clusters of 64 KiB -- into /vms/debian/disk.raw
...
hv: /vms/debian/disk.raw: 907 MiB written of 3072 MiB, the rest holes -- 14487 clusters inflated, 788 copied, 0 zeros, 33877 not there -- in 62.0 s
hv: /vms/debian/vm.toml written -- hv start debian boots it: hvboot, a 8192 MiB disk, cloud-init given 1 key and a password
```

Что значат слова команды:

- **`debian`** — имя ВМ, а её папка — `/vms/debian`.
- **Ссылка** ведёт на облачный образ Debian 13 generic. Его скачивает собственный HTTPS-клиент ядра: он проверяет сертификат сервера и идёт по редиректу Debian на зеркало.
- **`size=8G`** увеличивает диск дырой, которая ничего не стоит, пока гость в неё не пишет. При первой загрузке cloud-init растягивает на неё корневой раздел.
- **`password=`** даёт пользователю образа по умолчанию, `debian`, пароль для консоли. Без него войти можно только по вашему ключу.
- **`forward=2201:22`** пробрасывает порт 2201 в nos на порт 22 гостя, пока гость работает.
- **Остальные слова `hv start`** тоже попадают в файл: `mem=2048`, `cpus=2`, `port=3`, `cmdline=...` ([A VM of a cloud's image](/docs/hypervisor#a-vm-of-a-clouds-image)).

Посмотрите, что получилось:

```text
$ hv vms
debian  not running  hvboot  1024 MiB  1 cpu  disks disk.raw, seed.iso:ro  net  forward 2201:22  restart
$ ls /vms/debian
f 8589934592 disk.raw
f 65536 seed.iso
f 228 vm.toml
$ cat /vms/debian/vm.toml
# A VM of nos's hypervisor: hv start <this folder's name> boots it (docs/hypervisor.md)
kernel = "/lib/hvboot"
memory = 1024
disks = ["disk.raw", "seed.iso:ro"]
net = true
forward = ["2201:22"]
restart = true
console_log = true
```

`disk.raw` — это диск: длиной 8 ГиБ, из них записано 907 МиБ. `seed.iso` — сид для cloud-init, который гость видит как диск только для чтения с меткой `cidata`. `hv start` читает `vm.toml` при каждом запуске ВМ и отвергает незнакомый ключ, называя строку. Ключи перечислены в [Where the folders are](/docs/hypervisor#where-the-folders-are).

## 4. Запустите ВМ и войдите в неё

```text
$ hv start debian
hv: vm 0 (debian) started on cpu 3 -- /lib/hvboot, 1024 MiB, 1 cpu, cmdline "console=ttyS0 nolapic ip=10.0.100.2::10.0.100.1:255.255.255.0:::off:10.0.2.3", restarted when it resets
hv: port 2201 forwarded to vm 0, 10.0.100.2:22
```

hvboot находит собственный `/boot` гостя, читает его `grub.cfg` и загружает ядро пункта по умолчанию. Затем cloud-init настраивает гостя по сиду: имя машины, ваш ключ, пароль и увеличенный корень. `hv console debian` показывает, что гость успел напечатать. `ssh -t -p 2222 root@127.0.0.1 hv attach debian` следит за его консолью вживую и передаёт ей ваши нажатия, а `Ctrl-]` отключается. Под TCG на Mac sshd гостя ответил через 49 с после `hv start`. Войдите со своей машины:

```sh
ssh -p 2201 debian@127.0.0.1
```

Соединение идёт через проброс порта QEMU к nos, а затем через проброс nos к гостю. Внутри проверьте, что сделал cloud-init:

```text
debian@debian:~$ cloud-init status --long
status: done
...
detail: DataSourceNoCloud [seed=/dev/vdb]
errors: []
debian@debian:~$ df -h /
Filesystem      Size  Used Avail Use% Mounted on
/dev/vda1       7.7G  596M  6.8G   8% /
```

Пользователю `debian` доступен `sudo`. В консоли гостя (`hv attach debian`) тот же пользователь входит с паролем, который вы дали `hv create`.

## 5. Сохраните ВМ между перезагрузками

Папка лежит на диске nos, поэтому переживает nos. Чтобы ВМ запускалась при каждой загрузке nos, добавьте в `/etc/rc` три строки:

```text
$ rc add insmod /lib/modules/hv.ko
$ rc add hv on
$ rc add hv start debian
$ rc
1  insmod /lib/modules/sshd.ko
2  sshd start
3  insmod /lib/modules/hv.ko
4  hv on
5  hv start debian
```

Потом остановите гостя и nos:

```sh
hv stop debian
poweroff
```

`hv stop` нажимает кнопку питания гостя и даёт ему 30 с, чтобы выключиться самому; Debian под TCG хватило 7 с. `secs=N` меняет это время. Загрузите nos снова той же командой QEMU и ничего не набирайте: ВМ запустит `/etc/rc`. `ssh -p 2201 debian@127.0.0.1` попадёт на ту же машину, с тем же ключом хоста и теми же файлами. cloud-init не настраивает гостя второй раз, потому что instance ID в сиде не изменился. `/vms/debian/console.log` хранит консоль каждой загрузки, а между загрузками — строки самого hv:

```text
--- hv: 2026-10-25 12:46:37 UTC vm 0 stopped -- the guest powered itself off, S5 by its ACPI PM1 control register, at 0xffffffff96e1d0a3 ---
--- hv: 2026-10-25 12:46:37 UTC vm 0 off the list ---
--- hv: 2026-10-25 12:46:44 UTC vm 0 started on cpu 3 -- /lib/hvboot, 1024 MiB, 1 cpu, cmdline "...", restarted when it resets ---
```

На 8 МиБ журнал переезжает в `console.log.1`. Чтобы держать папки не в `/vms`, загрузите модуль с параметром: `insmod /lib/modules/hv.ko vms=<папка>`.

Чтобы удалить ВМ, остановите её, удалите её папку и её строку в `/etc/rc`:

```sh
hv stop debian
rm /vms/debian
rc del 5
```

## Другие образы

Та же команда берёт облачные образы других дистрибутивов, если hvboot может прочитать их `/boot`: ext2, ext3 или ext4 на диске с GPT или MBR и конфиг GRUB. Серверный образ Ubuntu 24.04 конвертируется в nos и загружается через hvboot, хотя целиком через `hv create` его ещё не создавали:

```sh
hv create noble https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img size=20G
```

Его загрузка весит 592 МиБ, а сконвертированный диск занимает 1,7 ГиБ, так что на корне nos должно хватить места на то и другое. Пока не поддерживаются: `/boot` на XFS или btrfs (семейство RHEL), на LVM или LUKS, BLS-записи GRUB и всё, чему для запуска ядра нужен EFI. Файл, который не qcow2 и не сырой диск, отвергается, а в сообщении сказано, что это за файл.

## На сервере

На сервере, установленном из системного образа, собранного из master ([Установка nos на выделенный сервер](/howto/install-on-a-server); образ v0.0.25 старше hvboot), `hv.ko` уже лежит в `/lib/modules`, hvboot — в `/lib/hvboot`, `/etc/rc` запускает sshd, а `nos-install --ssh-key` положил ваш ключ в `/etc/ssh/authorized_keys`. Шаги 3–5 там те же, а `forward=` делает гостя доступным по собственному адресу сервера. Раздел, с которого работает nos, должен вмещать образы и диски гостей. На серверах проекта `hv create` ещё не запускали.

## Если что-то пошло не так

**`the server answered 404 -- there is nothing at that URL`.** Образ переехал. Дистрибутивы переименовывают и убирают образы, поэтому берите ссылку со страницы загрузки дистрибутива.

**`its host has no address the resolver found -- is DNS on (dns=on)?`.** У nos нет DNS-сервера. Командная строка ISO включает DNS, так что проверьте, получил ли nos адрес по DHCP (`net`). В своей командной строке добавьте `dns=on`.

**`the TLS handshake was refused -- a certificate that did not verify (is the clock right?), or no protocol in common`.** Сертификат проходит проверку, только если часы nos идут верно, а время nos берёт из RTC.

**`writing the file failed -- is the filesystem full?`.** Корень nos мал для загрузки и диска вместе. Сделайте `root.img` побольше. Неудачный `hv create` удаляет всё, что успел сделать, так что его можно просто запустить снова.

**`/vms/debian/vm.toml is there already -- not written over`.** ВМ с таким именем уже есть. Выберите другое имя или сначала удалите старую ВМ.

**`hv: no key in /etc/ssh/authorized_keys and no password= -- nobody can log in to it`.** ВМ создана, но войти в неё никто не сможет. Удалите её, добавьте ключ командой `sshd allow <строка из вашего .pub-файла>` или передайте `password=` и создайте ВМ заново.

**`ssh -p 2201` отказывает в соединении или зависает.** Гость ещё грузится. `hv console debian` покажет, докуда он дошёл. Под TCG первая загрузка занимает около минуты.

## Что дальше

- [ВМ по ссылке](/blog/vms-from-cloud-images): как устроена каждая часть и как её проверяли.
- В справочнике: [A VM in a folder](/docs/hypervisor#a-vm-in-a-folder), [A VM of a cloud's image](/docs/hypervisor#a-vm-of-a-clouds-image), [A disk from a cloud's image](/docs/hypervisor#a-disk-from-a-clouds-image).
- [Linux-гость под гипервизором nos](/howto/linux-guest): гости из ядра, которое вы собрали сами, с дисками, сетью и несколькими CPU.
- `nos/scripts/hv-distro-test.py --create <url>` прогоняет это руководство как тест и проверяет гостя изнутри ([Гейты: как проверять изменения в nos](/howto/run-the-gates)).
