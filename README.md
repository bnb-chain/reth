# ‼️ IMPORTANT NOTE ‼️

**BNB Reth Client: Temporary Suspension of Development**

We are announcing that the BNB Reth client will suspend development and maintenance. Our team has released the final maintenance version [v1.1.1](https://github.com/bnb-chain/reth/releases/tag/v1.1.1) as the last supported release.

Key Points:
  - The final maintenance release includes stability improvements and minor bug fixes.
  - Existing users can continue running BSC-reth and op-reth until the BSC/opBNB Pectra upgrade.
  - The BSC Pectra upgrade is scheduled for testnet on February 21st, 2025, with mainnet upgrade expected in mid-March 2025

While the Reth client will remain functional until the Pectra upgrade, we recommend users to gradually transition to other supported BSC clients, like [Geth](https://github.com/bnb-chain/bsc) and [Erigon](https://github.com/node-real/bsc-erigon). Please ensure you have the latest version installed for optimal performance during the remaining operational period.

**We sincerely thank our community for their continued support throughout BNB Reth's development journey.**

*January 22nd, 2025*

-------

# BNB Chain Reth

[![CI status](https://github.com/paradigmxyz/reth/workflows/unit/badge.svg)][gh-ci]
[![cargo-deny status](https://github.com/paradigmxyz/reth/workflows/deny/badge.svg)][gh-deny]
[![Discord Chat][discord-badge]][discord-url]

[gh-ci]: https://github.com/bnb-chain/reth/actions/workflows/unit.yml

[gh-deny]: https://github.com/bnb-chain/reth/actions/workflows/deny.yml

[discord-badge]: https://img.shields.io/badge/discord-join%20chat-blue.svg

[discord-url]: https://discord.gg/z2VpC455eU

BNB Chain Reth is a blockchain client based on [Reth](https://github.com/paradigmxyz/reth/), designed to provide
seamless support for [BNB Smart Chain(BSC)](https://github.com/bnb-chain/bsc)
and [opBNB](https://github.com/bnb-chain/op-geth).

## Build from Source

For prerequisites and detailed build instructions please read
the [Installation Instructions](https://paradigmxyz.github.io/reth/installation/source.html).

With Rust and the dependencies installed, you're ready to build BNB Chain Reth. First, clone the repository:

```shell
git clone https://github.com/bnb-chain/reth.git
cd reth
```

In the realm of BSC, you have the option to execute the following commands to compile bsc-reth:

```shell
make build-bsc
```

Alternatively, you can install reth using the following command:

```shell
make install-bsc
```

When it comes to opBNB, you can run the following commands to compile op-reth:

```shell
make build-op
```

Or, opt for installing op-reth with the command:

```shell
make install-op
```

## Before setting up the node

### Optimizing `vm.min_free_kbytes` for MDBX Storage in Reth

#### Why Adjust `vm.min_free_kbytes`?

Reth uses **MDBX** as its underlying storage engine, which relies on **memory-mapped I/O (mmap)** for high-performance operations. However, MDBX can consume a significant amount of memory, and in scenarios where applications allocate memory aggressively, the system may run into **memory pressure**.

By increasing `vm.min_free_kbytes`, you can **prevent the Linux OOM (Out-Of-Memory) killer** from terminating essential processes when free memory runs low. This ensures smoother performance and better stability.

#### Recommended Setting

We recommend setting `vm.min_free_kbytes` to at least **4GB (4194304 kbytes)** to ensure system stability when using MDBX.

#### **Linux**

To apply the setting temporarily (until reboot):

```sh
sudo sysctl -w vm.min_free_kbytes=4194304
```

To make it persist across reboots, add the following line to /etc/sysctl.conf:

```sh
echo "vm.min_free_kbytes=4194304" | sudo tee -a /etc/sysctl.conf
```

Then apply the changes:

```sh
sudo sysctl -p
```

### Verifying the Configuration

To verify that the setting has been applied correctly, run:

```sh
cat /proc/sys/vm/min_free_kbytes
```

## Run Reth for BSC

### Hardware Requirements

* CPU with 16+ cores
* 128GB RAM
* High-performance NVMe SSD with at least 4TB of free space for full node and 8TB of free space for archive node
* A broadband internet connection with upload/download speeds of 25 MB/s

### Steps to Run bsc-reth

The command below is for an archive node. To run a full node, simply add the `--full` tag.

```shell
# for mainnet
export network=bsc

# for testnet
# export network=bsc-testnet

./target/release/bsc-reth node \
    --datadir=./datadir \
    --chain=${network} \
    --http \
    --http.api="eth, net, txpool, web3, rpc" \
    --log.file.directory ./datadir/logs \
    --enable-prefetch \
    --optimize.enable-execution-cache
```

You can run `bsc-reth --help` for command explanations.

For running bsc-reth with docker, please use the following command:

```shell
# for mainnet
export network=bsc

# for testnet
# export network=bsc-testnet

# check this for version of the docker image, https://github.com/bnb-chain/reth/pkgs/container/bsc-reth
export version=latest

# the directory where reth data will be stored
export data_dir=/xxx/xxx

docker run -d -p 8545:8545 -p 30303:30303 -p 30303:30303/udp -v ${data_dir}:/data \
    --name bsc-reth ghcr.io/bnb-chain/bsc-reth:${version} node \
    --datadir=/data \
    --chain=${network} \
    --http \
    --http.api="eth, net, txpool, web3, rpc" \
    --log.file.directory /data/logs \
    --enable-prefetch \
    --optimize.enable-execution-cache
```

### Snapshots

There are snapshots available from the community, you can use a snapshot to reduce the sync time for catching up.

* [fuzzland snapshot](https://github.com/fuzzland/snapshots)
* [bnb-chain snapshot](https://github.com/bnb-chain/reth-snapshots)

## Run Reth for opBNB

The op-reth can function as both a full node and an archive node. Due to its unique storage advantages, it is primarily
utilized for running archive nodes.

### Hardware Requirements

* CPU with 16+ cores
* 128GB RAM
* High-performance NVMe SSD with at least 3TB of free space
* A broadband internet connection with upload/download speeds of 25 MB/s

### Steps to Run op-reth

The op-reth is an [execution client](https://ethereum.org/en/developers/docs/nodes-and-clients/#execution-clients) for
opBNB. You need to run op-node along with op-reth to synchronize with the opBNB network.

Here is the quick command for running the op-node. For more details, refer to
the [opbnb repository](https://github.com/bnb-chain/opbnb).

```shell
git clone https://github.com/bnb-chain/opbnb
cd opbnb
make op-node

# for mainnet
export network=mainnet
export L1_RPC=https://bsc-dataseed.bnbchain.org
export P2P_BOOTNODES="enr:-J24QA9sgVxbZ0KoJ7-1gx_szfc7Oexzz7xL2iHS7VMHGj2QQaLc_IQZmFthywENgJWXbApj7tw7BiouKDOZD4noWEWGAYppffmvgmlkgnY0gmlwhDbjSM6Hb3BzdGFja4PMAQCJc2VjcDI1NmsxoQKetGQX7sXd4u8hZr6uayTZgHRDvGm36YaryqZkgnidS4N0Y3CCIyuDdWRwgiMs,enr:-J24QPSZMaGw3NhO6Ll25cawknKcOFLPjUnpy72HCkwqaHBKaaR9ylr-ejx20INZ69BLLj334aEqjNHKJeWhiAdVcn-GAYv28FmZgmlkgnY0gmlwhDTDWQOHb3BzdGFja4PMAQCJc2VjcDI1NmsxoQJ-_5GZKjs7jaB4TILdgC8EwnwyL3Qip89wmjnyjvDDwoN0Y3CCIyuDdWRwgiMs"

# for testnet
# it's better to replace the L1_RPC with your own BSC Testnet RPC Endpoint for stability
# export network=testnet
# export L1_RPC=https://bsc-testnet.bnbchain.org
# export P2P_BOOTNODES="enr:-J24QGQBeMsXOaCCaLWtNFSfb2Gv50DjGOKToH2HUTAIn9yXImowlRoMDNuPNhSBZNQGCCE8eAl5O3dsONuuQp5Qix2GAYjB7KHSgmlkgnY0gmlwhDREiqaHb3BzdGFja4PrKwCJc2VjcDI1NmsxoQL4I9wpEVDcUb8bLWu6V8iPoN5w8E8q-GrS5WUCygYUQ4N0Y3CCIyuDdWRwgiMr,enr:-J24QJKXHEkIhy0tmIk2EscMZ2aRrivNsZf_YhgIU51g4ZKHWY0BxW6VedRJ1jxmneW9v7JjldPOPpLkaNSo6cXGFxqGAYpK96oCgmlkgnY0gmlwhANzx96Hb3BzdGFja4PrKwCJc2VjcDI1NmsxoQMOCzUFffz04eyDrmkbaSCrMEvLvn5O4RZaZ5k1GV4wa4N0Y3CCIyuDdWRwgiMr"

./op-node/bin/op-node \
  --l1.trustrpc \
  --sequencer.l1-confs=15 \
  --verifier.l1-confs=15 \
  --l1.http-poll-interval 60s \
  --l1.epoch-poll-interval 180s \
  --l1.rpc-max-batch-size 20 \
  --rollup.config=./assets/${network}/rollup.json \
  --rpc.addr=0.0.0.0 \
  --rpc.port=8546 \
  --p2p.sync.req-resp \
  --p2p.listen.ip=0.0.0.0 \
  --p2p.listen.tcp=9003 \
  --p2p.listen.udp=9003 \
  --snapshotlog.file=./snapshot.log \
  --p2p.bootnodes=$P2P_BOOTNODES \
  --metrics.enabled \
  --metrics.addr=0.0.0.0 \
  --metrics.port=7300 \
  --pprof.enabled \
  --rpc.enable-admin \
  --l1=${L1_RPC} \
  --l2=http://localhost:8551 \
  --l2.jwt-secret=./jwt.txt \
  --syncmode=execution-layer
```

**It's important to mention that op-node and op-reth both need the same jwt.txt file.**
To do this, switch to the op-reth workdir and paste the jwt.txt file created during op-node execution into the current
workspace.

Here is a quick command for running op-reth. The command below is for an archive node, to run a full node, simply add
the `--full` tag.

```shell
# for mainnet
export network=mainnet
export L2_RPC=https://opbnb-mainnet-rpc.bnbchain.org

# for testnet
# export network=testnet
# export L2_RPC=https://opbnb-testnet-rpc.bnbchain.org

./target/release/op-reth node \
    --datadir=./datadir \
    --chain=opbnb-${network} \
    --rollup.sequencer-http=${L2_RPC} \
    --authrpc.addr="0.0.0.0" \
    --authrpc.port=8551 \
    --authrpc.jwtsecret=./jwt.txt \
    --http \
    --http.api="eth, net, txpool, web3, rpc" \
    --log.file.directory ./datadir/logs \
    --enable-prefetch \
    --optimize.enable-execution-cache
```

You can run `op-reth --help` for command explanations. More details on running opbnb nodes can be
found [here](https://docs.bnbchain.org/opbnb-docs/docs/tutorials/running-a-local-node/).

For running op-reth with docker, please use the following command:

```shell
# for mainnet
export network=mainnet
export L2_RPC=https://opbnb-mainnet-rpc.bnbchain.org

# for testnet
# export network=testnet
# export L2_RPC=https://opbnb-testnet-rpc.bnbchain.org

# check this for version of the docker image, https://github.com/bnb-chain/reth/pkgs/container/op-reth
export version=latest

# the directory where reth data will be stored
export data_dir=/xxx/xxx

# the directory where the jwt.txt file is stored
export jwt_dir=/xxx/xxx

docker run -d -p 8545:8545 -p 30303:30303 -p 30303:30303/udp -v ${data_dir}:/data -v ${jwt_dir}:/jwt \
    --name op-reth ghcr.io/bnb-chain/op-reth:${version} node \
    --datadir=/data \
    --chain=opbnb-${network} \
    --rollup.sequencer-http=${L2_RPC} \
    --authrpc.addr="0.0.0.0" \
    --authrpc.port=8551 \
    --authrpc.jwtsecret=/jwt/jwt.txt \
    --http \
    --http.api="eth, net, txpool, web3, rpc" \
    --log.file.directory /data/logs \
    --enable-prefetch \
    --optimize.enable-execution-cache
```

## Contribution

Thank you for considering helping out with the source code! We welcome contributions
from anyone on the internet, and are grateful for even the smallest of fixes!

If you'd like to contribute to bnb chain reth, please fork, fix, commit and send a pull request
for the maintainers to review and merge into the main code base. If you wish to submit
more complex changes though, please check up with the core devs first
on [our discord channel](https://discord.gg/bnbchain)
to ensure those changes are in line with the general philosophy of the project and/or get
some early feedback which can make both your efforts much lighter as well as our review
and merge procedures quick and simple.

Please see the [Developers' Guide](https://github.com/bnb-chain/reth/tree/develop/docs)
for more details on configuring your environment, managing project dependencies, and
testing procedures.
