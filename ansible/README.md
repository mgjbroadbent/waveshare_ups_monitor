# Ansible

Deploys the daemon to one or more Raspberry Pis. The role does the work and is documented in
[`roles/waveshare_ups/README.md`](roles/waveshare_ups/README.md); this directory is just a runnable
example around it.

```sh
# 1. Build the binary for the Pi (see ../README.md#build for why zigbuild)
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.28

# 2. Point Ansible at your Pi
cp inventory.example.ini inventory.ini && $EDITOR inventory.ini

# 3. Set your broker, then deploy
$EDITOR playbook.yml     # waveshare_ups_mqtt_host
ansible-playbook -i inventory.ini playbook.yml

# 4. Watch it
ansible -i inventory.ini waveshare_ups -b -a 'journalctl -u waveshare-ups -n 20'
```

`playbook.yml` ships the binary from `target/` outside the role. To ship it *inside* the role
instead, copy it to `roles/waveshare_ups/files/` and drop the `waveshare_ups_binary_src` line — see
[Shipping the executable](roles/waveshare_ups/README.md#shipping-the-executable).

Before a first run against real hardware, note that the default hooks include
[`battery-critical.sh`](roles/waveshare_ups/files/hooks/battery-critical.sh), which powers the
machine off at 5%.
