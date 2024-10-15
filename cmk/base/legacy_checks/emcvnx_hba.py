#!/usr/bin/env python3
# Copyright (C) 2019 Checkmk GmbH - License: GNU General Public License v2
# This file is part of Checkmk (https://checkmk.com). It is subject to the terms and
# conditions defined in the file COPYING, which is part of this source code package.

# Example output from agent:
# <<<emcvnx_hba>>>
# Information about each SPPORT:
#
# SP Name:             SP A
# SP Port ID:          0
# SP UID:              50:06:01:60:BE:A0:5D:E5:50:06:01:60:3E:A0:5D:E5
# Link Status:         Up
# Port Status:         Online
# Switch Present:      YES
# Switch UID:          10:00:00:27:F8:28:52:5B:20:02:00:27:F8:28:52:5B
# SP Source ID:        66048
# ALPA Value:         0
# Speed Value :         8Gbps
# Auto Negotiable :     NO
# Available Speeds:
# 2Gbps
# 4Gbps
# 8Gbps
# Auto
# Requested Value:      Auto
# MAC Address:         Not Applicable
# SFP State:           Online
# Reads:               426729
# Writes:              8683578
# Blocks Read:         4917783
# Blocks Written:      12008476
# Queue Full/Busy:     0
# I/O Module Slot:     Onboard
# Physical Port ID:    2
# Usage:     Mirrorview
# SFP/Connector EMC Part Number: 019-078-042
# SFP/Connector EMC Serial Number: 00000000000
# SFP/Connector Vendor Part Number: AFBR-57D7APZ-E2
# SFP/Connector Vendor Serial Number: AGL1213A3188822
# SFP/Connector Supported Speeds:
# 2Gbps
# 4Gbps
# 8Gbps
#
# SP Name:             SP A
# SP Port ID:          1
# SP UID:              50:06:01:60:BE:A0:5D:E5:50:06:01:61:3E:A0:5D:E5
# Link Status:         Up
# Port Status:         Online
# Switch Present:      YES
# [...]

# Parse agent output into a dict of the form:
# parsed = {
# {'SP A Port 0': {'Blocks Read': 4917783, 'Blocks Written': 12008476},
#  'SP A Port 1': {'Blocks Read': 363283639, 'Blocks Written': 218463965},
#  'SP A Port 2': {'Blocks Read': 2, 'Blocks Written': 0},
#  'SP B Port 0': {'Blocks Read': 0, 'Blocks Written': 4348086},
# }


# mypy: disable-error-code="var-annotated"

import time

from cmk.base.config import check_info

from cmk.agent_based.v0_unstable_legacy import LegacyCheckDefinition
from cmk.agent_based.v2 import get_rate, get_value_store


def saveint(i: str) -> int:
    """Tries to cast a string to an integer and return it. In case this
    fails, it returns 0.

    Advice: Please don't use this function in new code. It is understood as
    bad style these days, because in case you get 0 back from this function,
    you can not know whether it is really 0 or something went wrong."""
    try:
        return int(i)
    except (TypeError, ValueError):
        return 0


def parse_emcvnx_hba(string_table):
    parsed = {}
    for line in string_table:
        if len(line) > 2 and line[0] == "SP" and line[1] == "Name:":
            hba_id = " ".join(line[2:])
        elif len(line) > 2 and line[0] == "SP" and line[1] == "Port" and line[2] == "ID:":
            hba_id += " Port " + line[-1]
            hba = {}
            parsed[hba_id] = hba
        elif len(line) > 2 and line[0] == "Blocks" and line[1] in ("Read:", "Written:"):
            hba["Blocks " + line[1].replace(":", "")] = saveint(line[-1])
    return parsed


def inventory_emcvnx_hba(parsed):
    for hba, values in parsed.items():
        # Old Versions of EMC don't have any Information
        if values:
            yield hba, None


def check_emcvnx_hba(item, _no_params, parsed):
    now = time.time()
    perfdata = []
    if item not in parsed:
        return 3, "HBA %s not found in agent output" % item

    read_blocks = parsed[item]["Blocks Read"]
    write_blocks = parsed[item]["Blocks Written"]
    countername_r = "emcvnx_hba.read_blocks.%s" % item.replace(" ", "_")
    countername_w = "emcvnx_hba.write_blocks.%s" % item.replace(" ", "_")

    read_blocks_per_sec = get_rate(
        get_value_store(), countername_r, now, read_blocks, raise_overflow=True
    )
    write_blocks_per_sec = get_rate(
        get_value_store(), countername_w, now, write_blocks, raise_overflow=True
    )
    perfdata.append(("read_blocks", read_blocks_per_sec))
    perfdata.append(("write_blocks", write_blocks_per_sec))

    return (
        0,
        f"Read: {read_blocks_per_sec:.2f} Blocks/s, Write: {write_blocks_per_sec:.2f} Blocks/s",
        perfdata,
    )


check_info["emcvnx_hba"] = LegacyCheckDefinition(
    name="emcvnx_hba",
    parse_function=parse_emcvnx_hba,
    service_name="HBA %s",
    discovery_function=inventory_emcvnx_hba,
    check_function=check_emcvnx_hba,
)
