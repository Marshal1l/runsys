sudo rm -rf /run/runsys
cargo build;
sudo ./target/debug/runsys create 1 ~/RUNC/runsys/busybox-test
sudo ./target/debug/runsys start 1 