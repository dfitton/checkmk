#!/usr/bin/env python3
# Copyright (C) 2020 Checkmk GmbH - License: GNU General Public License v2
# This file is part of Checkmk (https://checkmk.com). It is subject to the terms and
# conditions defined in the file COPYING, which is part of this source code package.

from collections.abc import Sequence

import pytest

from livestatus import SiteId

from cmk.utils.exceptions import MKGeneralException
from cmk.utils.hostaddress import HostName

import cmk.gui.graphing._graph_templates as gt
from cmk.gui.graphing._graph_specification import (
    GraphConsoldiationFunction,
    GraphMetric,
    MetricDefinition,
    MetricExpression,
    RPNExpression,
    RPNExpressionConstant,
    RPNExpressionOperator,
    RPNExpressionRRD,
)
from cmk.gui.graphing._utils import GraphRecipeBase, GraphTemplate
from cmk.gui.metrics import translate_perf_data


@pytest.mark.parametrize(
    "expression, enforced_consolidation_function, result",
    [
        ("user", "max", [("user", "max")]),
        ("user.min", None, [("user", "min")]),
        ("user.min,sys,+", None, [("user", "min"), ("sys", None), ("+", None)]),
        ("user.min,sys.max,+", None, [("user", "min"), ("sys", "max"), ("+", None)]),
    ],
)
def test_rpn_consolidation(
    expression: MetricExpression,
    enforced_consolidation_function: GraphConsoldiationFunction | None,
    result: Sequence[tuple[str, GraphConsoldiationFunction | None]],
) -> None:
    assert list(gt.iter_rpn_expression(expression, enforced_consolidation_function)) == result


@pytest.mark.parametrize(
    "expression, enforced_consolidation_function", [("user.min", "max"), ("user.min,sys,+", "avg")]
)
def test_rpn_consolidation_exception(
    expression: MetricExpression, enforced_consolidation_function: GraphConsoldiationFunction | None
) -> None:
    with pytest.raises(MKGeneralException):
        list(gt.iter_rpn_expression(expression, enforced_consolidation_function))


@pytest.mark.parametrize(
    "expression, result",
    [
        (
            "fs_size,fs_used,-",
            RPNExpressionOperator(
                "-",
                [
                    RPNExpressionRRD(SiteId(""), HostName(""), "", "fs_size", None, 1048576),
                    RPNExpressionRRD(SiteId(""), HostName(""), "", "_", None, 1048576),
                ],
            ),
        ),
        (
            "fs_growth.min,0,MIN,-1,*",
            RPNExpressionOperator(
                "*",
                [
                    RPNExpressionOperator(
                        "MIN",
                        [
                            RPNExpressionRRD(
                                SiteId(""), HostName(""), "", "growth", "min", 12.136296296296296
                            ),
                            RPNExpressionConstant(0.0),
                        ],
                    ),
                    RPNExpressionConstant(-1.0),
                ],
            ),
        ),
    ],
)
def test_rpn_stack(expression: MetricExpression, result: RPNExpression) -> None:
    translated_metrics = translate_perf_data(
        "/=163651.992188;;;; fs_size=477500.03125;;;; growth=-1280.489081;;;;", "check_mk-df"
    )
    lq_row = {"site": "", "host_name": "", "service_description": ""}
    assert (
        gt.metric_expression_to_graph_recipe_expression(
            expression, translated_metrics, lq_row, None
        )
        == result
    )


def test_create_graph_recipe_from_template() -> None:
    graph_template = GraphTemplate(
        id="my_id",
        title=None,
        metrics=[
            MetricDefinition(expression="fs_used", line_type="area"),
            MetricDefinition(
                expression="fs_size,fs_used,-#e3fff9",
                line_type="stack",
                title="Free space",
            ),
            MetricDefinition(expression="fs_size", line_type="line"),
        ],
        scalars=[
            "fs_used:warn",
            "fs_used:crit",
        ],
        conflicting_metrics=["fs_free"],
        optional_metrics=[],
        consolidation_function=None,
        range=(0, "fs_used:max"),
        omit_zero_metrics=False,
    )
    translated_metrics = translate_perf_data(
        "/=163651.992188;;;; fs_size=477500.03125;;;; growth=-1280.489081;;;;", "check_mk-df"
    )
    lq_row = {"site": "", "host_name": "", "service_description": ""}

    assert gt.create_graph_recipe_from_template(
        graph_template, translated_metrics, lq_row
    ) == GraphRecipeBase(
        title="Used space",
        metrics=[
            GraphMetric(
                unit="bytes",
                color="#00ffc6",
                title="Used space",
                line_type="area",
                expression=RPNExpressionRRD(SiteId(""), HostName(""), "", "_", "max", 1048576),
                visible=True,
            ),
            GraphMetric(
                unit="bytes",
                color="#e3fff9",
                title="Free space",
                line_type="stack",
                expression=RPNExpressionOperator(
                    "-",
                    [
                        RPNExpressionRRD(SiteId(""), HostName(""), "", "fs_size", "max", 1048576),
                        RPNExpressionRRD(SiteId(""), HostName(""), "", "_", "max", 1048576),
                    ],
                ),
                visible=True,
            ),
            GraphMetric(
                unit="bytes",
                color="#006040",
                title="Total size",
                line_type="line",
                expression=RPNExpressionRRD(
                    SiteId(""), HostName(""), "", "fs_size", "max", 1048576
                ),
                visible=True,
            ),
        ],
        unit="bytes",
        explicit_vertical_range=(0.0, None),
        horizontal_rules=[],
        omit_zero_metrics=False,
        consolidation_function="max",
    )


@pytest.mark.parametrize(
    "expression, perf_string, check_command, result_color",
    [
        (
            "load15",
            "load1=0.38;40;80;0;8 load5=0.62;40;80;0;8 load15=0.68;40;80;0;8",
            "check_mk-cpu.loads",
            "#2c5766",
        ),
        ("test", "test=5;5;10;0;20", "check_mk-local", "#cc00ff"),
    ],
)
def test_metric_unit_color(
    expression: str, perf_string: str, check_command: str | None, result_color: str
) -> None:
    translated_metrics = translate_perf_data(perf_string, check_command)
    translated_metric = translated_metrics.get(expression)
    assert translated_metric is not None
    unit = translated_metric.get("unit")
    assert unit is not None
    unit_id = unit.get("id")
    reference = {
        "color": result_color,
        "unit": unit_id,
    }
    assert gt.metric_unit_color(expression, translated_metrics, ["test"]) == reference


@pytest.mark.parametrize(
    "expression, perf_string, check_command",
    [
        ("test", "", "check_mk-local"),
    ],
)
def test_metric_unit_color_skip(
    expression: MetricExpression, perf_string: str, check_command: str | None
) -> None:
    translated_metrics = translate_perf_data(perf_string, check_command)
    assert gt.metric_unit_color(expression, translated_metrics, ["test"]) is None


@pytest.mark.parametrize(
    "metric, perf_string, check_command",
    [
        ("level,altitude,+", "test=5;5;10;0;20", "check_mk-local"),
    ],
)
def test_metric_unit_color_exception(
    metric: MetricExpression, perf_string: str, check_command: str | None
) -> None:
    translated_metrics = translate_perf_data(perf_string, check_command)
    with pytest.raises(MKGeneralException):
        gt.metric_unit_color(metric, translated_metrics, ["test"])
