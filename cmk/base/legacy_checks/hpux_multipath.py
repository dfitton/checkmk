#!/usr/bin/env python3
# Copyright (C) 2019 Checkmk GmbH - License: GNU General Public License v2
# This file is part of Checkmk (https://checkmk.com). It is subject to the terms and
# conditions defined in the file COPYING, which is part of this source code package.

# Example output from agent:
# <<<hpux_multipath>>>
#       LUN PATH INFORMATION FOR LUN : /dev/rtape/tape1_BEST
# World Wide Identifier(WWID)    = 0x600508b4000139e500049000075e0000
# State                         = UNOPEN
#       LUN PATH INFORMATION FOR LUN : /dev/rdisk/disk10
# World Wide Identifier(WWID)    = 0x600508b4000139e500009000075e00b0
# State                         = ACTIVE
#       LUN PATH INFORMATION FOR LUN : /dev/rdisk/disk13
# World Wide Identifier(WWID)    = 0x600508b4000139e500009000075e00c0
# State                         = UNOPEN
#       LUN PATH INFORMATION FOR LUN : /dev/pt/pt2
# World Wide Identifier(WWID)    = 0x600508b4000139e500009000075e00d0
# State                         = UNOPEN
# State                         = UNOPEN
# State                         = UNOPEN
# State                         = UNOPEN
# State                         = UNOPEN
# State                         = UNOPEN
# State                         = UNOPEN
# State                         = UNOPEN
#         LUN PATH INFORMATION FOR LUN : /dev/rdisk/disk781
# World Wide Identifier(WWID)    = 0x600508b4000139e500009000075e00e0
# State                         = ACTIVE
# State                         = STANDBY
# State                         = FAILED
# State                         = FAILED
# State                         = ACTIVE
# State                         = STANDBY
#       LUN PATH INFORMATION FOR LUN : /dev/rdisk/disk912
# World Wide Identifier(WWID)    = 0x600508b4000139e500009000075e00f0
# State                         = ACTIVE
# State                         = STANDBY
# State                         = ACTIVE
# State                         = STANDBY


from cmk.base.config import check_info

from cmk.agent_based.v0_unstable_legacy import LegacyCheckDefinition

hpux_multipath_pathstates = {
    "ACTIVE": 0,
    "STANDBY": 1,
    "FAILED": 2,
    "UNOPEN": 3,
    "OPENING": 0,
    "CLOSING": 1,
}


def parse_hpux_multipath(info):
    disks = {}
    for line in info:
        if ":" in line:
            disk = line[-1]
        elif line[0] == "World":
            wwid = line[-1]
            paths = [0, 0, 0, 0]  # ACTIVE, STANBY, FAILED, UNOPEN
            disks[wwid] = (disk, paths)
        elif "=" in line:
            state = line[-1]
            paths[hpux_multipath_pathstates[state]] += 1
    return disks


def inventory_hpux_multipath(parsed):
    for wwid, (_disk, (active, standby, failed, unopen)) in parsed.items():
        if active + standby + failed >= 2:
            yield wwid, {"expected": (active, standby, failed, unopen)}


def hpux_multipath_format_pathstatus(pathcounts):
    infos = []
    for name, i in hpux_multipath_pathstates.items():
        c = pathcounts[i]
        if c > 0:
            infos.append("%d %s" % (c, name))
    return ", ".join(infos)


def check_hpux_multipath(item, params, parsed):
    try:
        disk, pathcounts = parsed[item]
    except KeyError:
        return

    if pathcounts[2] > 0:
        yield (
            2,
            "%s: %d failed paths! (%s)"
            % (disk, pathcounts[2], hpux_multipath_format_pathstatus(pathcounts)),
        )
        return

    expected = params["expected"]
    if list(pathcounts) != list(expected):
        yield (
            1,
            "%s: Invalid path status %s (should be %s)"
            % (
                disk,
                hpux_multipath_format_pathstatus(pathcounts),
                hpux_multipath_format_pathstatus(expected),
            ),
        )
    else:
        yield 0, f"{disk}: {hpux_multipath_format_pathstatus(pathcounts)}"


check_info["hpux_multipath"] = LegacyCheckDefinition(
    name="hpux_multipath",
    service_name="Multipath %s",
    parse_function=parse_hpux_multipath,
    discovery_function=inventory_hpux_multipath,
    check_function=check_hpux_multipath,
    check_ruleset_name="hpux_multipath",
)
