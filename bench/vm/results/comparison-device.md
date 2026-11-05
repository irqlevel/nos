# A device's comparison: the guest on a partition of its own

`compare-n.py` over KVM on the raw partition (the reference), nos on it given by its id, and the file-disk runs before it -- KVM's of 31 October, nos's last fio (`dc529775-fio`) and last whole suite (`a0ae873e`). One run each; read the 4k random rows across runs, as the README says.

| metric | kvm-device | nos-21b67b1e-device (% of kvm-device) | kvm (% of kvm-device) | nos-dc529775-fio (% of kvm-device) | nos-a0ae873e (% of kvm-device) |
|---|---:|---:|---:|---:|---:|
| fio layout_seqwrite_1m_qd8, MiB/s | 4 762 | 4 633 (97%) | 4 643 (98%) | 3 696 (78%) | 3 007 (63%) |
| fio randread_4k_qd1, IOPS | 20 554 | 20 789 (101%) | 20 132 (98%) | 20 266 (99%) | 17 207 (84%) |
| fio randread_4k_qd1, avg latency us | 46.0 | 43.0 (107%) | 47.0 (98%) | 44.0 (105%) | 52.0 (88%) |
| fio randread_4k_qd1, p99 latency us | 51.0 | 47.0 (109%) | 52.0 (98%) | 49.0 (104%) | 59.0 (86%) |
| fio randread_4k_qd32, IOPS | 219 872 | 324 310 (147%) | 195 298 (89%) | 293 797 (134%) | 141 550 (64%) |
| fio randread_4k_qd32, avg latency us | 144 | 97.0 (148%) | 162 (89%) | 107 (135%) | 219 (66%) |
| fio randread_4k_qd32, p99 latency us | 209 | 146 (143%) | 232 (90%) | 154 (136%) | 242 (86%) |
| fio randread_4k_4x16, IOPS | 240 180 | 365 311 (152%) | 216 892 (90%) | 336 707 (140%) | 368 817 (154%) |
| fio randread_4k_4x16, avg latency us | 265 | 172 (154%) | 293 (90%) | 187 (142%) | 166 (160%) |
| fio randread_4k_4x16, p99 latency us | 325 | 248 (131%) | 358 (91%) | 264 (123%) | 1 269 (26%) |
| fio randwrite_4k_qd1, IOPS | 50 839 | 45 889 (90%) | 39 853 (78%) | 44 936 (88%) | 28 901 (57%) |
| fio randwrite_4k_qd1, avg latency us | 18.0 | 16.0 (112%) | 23.0 (78%) | 17.0 (106%) | 27.0 (67%) |
| fio randwrite_4k_qd1, p99 latency us | 20.0 | 21.0 (95%) | 67.0 (30%) | 21.0 (95%) | 35.0 (57%) |
| fio randwrite_4k_qd32, IOPS | 237 859 | 168 676 (71%) | 177 723 (75%) | 160 102 (67%) | 126 008 (53%) |
| fio randwrite_4k_qd32, avg latency us | 133 | 184 (72%) | 178 (75%) | 194 (69%) | 246 (54%) |
| fio randwrite_4k_qd32, p99 latency us | 211 | 211 (100%) | 288 (73%) | 209 (101%) | 264 (80%) |
| fio read_1m_qd8, MiB/s | 2 185 | 2 194 (100%) | 1 456 (67%) | 2 172 (99%) | 2 185 (100%) |
| fio read_1m_qd8, avg latency us | 3 625 | 3 626 (100%) | 5 452 (66%) | 3 664 (99%) | 3 607 (100%) |
| fio read_1m_qd8, p99 latency us | 4 112 | 4 014 (102%) | 6 324 (65%) | 4 079 (101%) | 4 014 (102%) |
| fio write_1m_qd8, MiB/s | 4 867 | 4 644 (95%) | 3 694 (76%) | 4 695 (96%) | 3 822 (79%) |
| fio write_1m_qd8, avg latency us | 1 601 | 1 692 (95%) | 2 124 (75%) | 1 674 (96%) | 1 846 (87%) |
| fio write_1m_qd8, p99 latency us | 5 406 | 5 472 (99%) | 5 603 (96%) | 5 799 (93%) | 4 947 (109%) |
| fio randwrite_4k_fdatasync, IOPS | 634 | 758 (120%) | 620 (98%) | 726 (115%) | 708 (112%) |
| fio randwrite_4k_fdatasync, avg latency us | 107 | 21.0 (510%) | 139 (77%) | 24.0 (446%) | 40.0 (268%) |
| fio randwrite_4k_fdatasync, p99 latency us | 138 | 24.0 (575%) | 166 (83%) | 27.0 (511%) | 60.0 (230%) |
| kernel untar, s | 6.05 | 6.02 (101%) | 6.14 (98%) | -- | 6.65 (91%) |
| kernel make_j4, s | 318 | 320 (99%) | 318 (100%) | -- | 331 (96%) |
| kernel rm, s | 0.812 | 0.806 (101%) | 0.8 (101%) | -- | 0.812 (100%) |
| pgbench init_s100, s | 6.22 | 6.69 (93%) | 6.54 (95%) | -- | 8.83 (70%) |
| pgbench ro c1, tps | 62 367 (±3%) | 60 950 (98%) | 61 626 (99%) | -- | 52 712 (±2%) (85%) |
| pgbench ro c1, latency ms | 0.016 (±3%) | 0.016 (±3%) (100%) | 0.016 (100%) | -- | 0.019 (±3%) (84%) |
| pgbench ro c4, tps | 270 243 | 284 402 (105%) | 271 761 (101%) | -- | 272 329 (101%) |
| pgbench ro c4, latency ms | 0.015 | 0.014 (107%) | 0.015 (100%) | -- | 0.015 (100%) |
| pgbench ro c8, tps | 257 720 | 268 649 (104%) | 256 393 (99%) | -- | 256 402 (99%) |
| pgbench ro c8, latency ms | 0.031 | 0.03 (103%) | 0.031 (100%) | -- | 0.031 (100%) |
| pgbench ro c16, tps | 236 252 | 244 905 (104%) | 237 714 (101%) | -- | 234 527 (99%) |
| pgbench ro c16, latency ms | 0.068 | 0.065 (105%) | 0.067 (101%) | -- | 0.068 (100%) |
| pgbench rw c1, tps | 423 | 679 (161%) | 411 (±2%) (97%) | -- | 638 (151%) |
| pgbench rw c1, latency ms | 2.37 | 1.47 (161%) | 2.43 (±2%) (97%) | -- | 1.57 (151%) |
| pgbench rw c4, tps | 1 232 | 1 459 (118%) | 1 211 (98%) | -- | 1 371 (111%) |
| pgbench rw c4, latency ms | 3.25 | 2.74 (118%) | 3.3 (98%) | -- | 2.92 (111%) |
| pgbench rw c8, tps | 2 389 | 2 786 (117%) | 2 353 (±14%) (99%) | -- | 2 637 (110%) |
| pgbench rw c8, latency ms | 3.35 | 2.87 (117%) | 3.4 (±20%) (99%) | -- | 3.03 (110%) |
| pgbench rw c16, tps | 4 679 | 5 182 (111%) | 4 655 (±2%) (99%) | -- | 4 950 (106%) |
| pgbench rw c16, latency ms | 3.42 | 3.09 (111%) | 3.44 (±2%) (99%) | -- | 3.23 (106%) |
