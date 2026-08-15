# Runner requirements

The Linux filesystem hard gate needs host-level loop devices, device-mapper and native filesystem mounts. A container-only executor is insufficient. Use a Linux VM/machine executor or an equivalent self-hosted Linux runner with sudo and the required kernel facilities.
