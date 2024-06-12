#!/usr/bin/env python3
"""FT benchmark runner

This script allow to run FT benchmark on remote GCP machine provisioned with `crt-benchmark`
Terraform script.
"""

import pathlib
import sys

import argparse

# Necessary to import `mocknet` below.
sys.path.append(str(pathlib.Path(__file__).resolve().parents[2] / 'lib'))

import mocknet


def main():
    # Parse arguments.
    parser = argparse.ArgumentParser(description="Run FT benchmark")
    parser.add_argument(
        "--unique-id",
        type=str,
        required=True,
        help="Name of the instance Terraform instance to run the benchmark on")

    args = parser.parse_args()
    print(args)

    all_nodes = mocknet.get_nodes(pattern="crt-benchmark-ft")
    traffic_generator = None
    nodes = []
    for n in all_nodes:
        if n.instance_name.endswith("traffic"):
            if traffic_generator is not None:
                sys.exit(
                    f'more than one traffic generator instance found. {traffic_generator.instance_name} and {n.instance_name}'
                )
            traffic_generator = n
        else:
            nodes.append(n)

    mocknet.clear_data(nodes)
    mocknet.init_validator_key(nodes[0])

    # mocknet.create_and_upload_genesis_file_from_empty_genesis(
    #     [(node, 1) for node in nodes],
    #     [],
    #     chain_id="mocknet",
    # )
    mocknet.create_and_upload_config_file_from_default(nodes, "mocknet")
    mocknet.start_nodes(nodes)
    # Start network.
    # Start loadtest.
    # Wait until it completes.
    # Shut down everything.


if __name__ == "__main__":
    main()
