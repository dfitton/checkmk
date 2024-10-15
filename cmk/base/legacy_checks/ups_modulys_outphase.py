#!/usr/bin/env python3
# Copyright (C) 2019 Checkmk GmbH - License: GNU General Public License v2
# This file is part of Checkmk (https://checkmk.com). It is subject to the terms and
# conditions defined in the file COPYING, which is part of this source code package.


from cmk.base.check_legacy_includes.elphase import check_elphase
from cmk.base.config import check_info

from cmk.agent_based.v0_unstable_legacy import LegacyCheckDefinition
from cmk.agent_based.v2 import SNMPTree
from cmk.plugins.lib.ups_modulys import DETECT_UPS_MODULYS


def parse_ups_modulys_outphase(string_table):
    if not string_table:
        return None

    parsed = {}
    parsed["Phase 1"] = {
        "frequency": int(string_table[0][1]) / 10.0,
        "voltage": int(string_table[0][3]) / 10.0,
        "current": int(string_table[0][4]) / 10.0,
        "power": int(string_table[0][5]),
        "output_load": int(string_table[0][6]),
    }

    if string_table[0][2] == "3":
        parsed["Phase 2"] = {
            "frequency": int(string_table[0][1]) / 10.0,
            "voltage": int(string_table[0][7]) / 10.0,
            "current": int(string_table[0][8]) / 10.0,
            "power": int(string_table[0][9]),
            "output_load": int(string_table[0][10]),
        }

        parsed["Phase 3"] = {
            "frequency": int(string_table[0][1]) / 10.0,
            "voltage": int(string_table[0][11]) / 10.0,
            "current": int(string_table[0][12]) / 10.0,
            "power": int(string_table[0][13]),
            "output_load": int(string_table[0][14]),
        }

    return parsed


def discover_ups_modulys_outphase(section):
    yield from ((item, {}) for item in section)


check_info["ups_modulys_outphase"] = LegacyCheckDefinition(
    name="ups_modulys_outphase",
    detect=DETECT_UPS_MODULYS,
    fetch=SNMPTree(
        base=".1.3.6.1.4.1.2254.2.4.5",
        oids=["1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15"],
    ),
    parse_function=parse_ups_modulys_outphase,
    service_name="Output %s",
    discovery_function=discover_ups_modulys_outphase,
    check_function=check_elphase,
    check_ruleset_name="ups_outphase",
    check_default_parameters={},
)
