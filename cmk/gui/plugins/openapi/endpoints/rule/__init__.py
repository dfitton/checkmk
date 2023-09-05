#!/usr/bin/env python3
# Copyright (C) 2020 Checkmk GmbH - License: GNU General Public License v2
# This file is part of Checkmk (https://checkmk.com). It is subject to the terms and
# conditions defined in the file COPYING, which is part of this source code package.
"""Rules"""
from __future__ import annotations

import dataclasses
import typing

from cmk.utils.datastructures import denilled
from cmk.utils.object_diff import make_diff_text
from cmk.utils.rulesets.ruleset_matcher import RuleOptionsSpec

from cmk.gui import exceptions, http
from cmk.gui.i18n import _l
from cmk.gui.logged_in import user
from cmk.gui.plugins.openapi.endpoints.rule.fields import (
    InputRuleObject,
    MoveRuleTo,
    RULE_ID,
    RuleCollection,
    RuleObject,
    RuleSearchOptions,
    UpdateRuleObject,
)
from cmk.gui.plugins.openapi.restful_objects import constructors, Endpoint, permissions
from cmk.gui.plugins.openapi.restful_objects.type_defs import DomainObject
from cmk.gui.plugins.openapi.utils import (
    problem,
    ProblemException,
    RestAPIRequestDataValidationException,
    serve_json,
)
from cmk.gui.utils import gen_id
from cmk.gui.utils.escaping import strip_tags
from cmk.gui.watolib.changes import add_change
from cmk.gui.watolib.hosts_and_folders import Folder
from cmk.gui.watolib.rulesets import (
    AllRulesets,
    FolderRulesets,
    Rule,
    RuleConditions,
    RuleOptions,
    Ruleset,
    RulesetCollection,
    visible_ruleset,
    visible_rulesets,
)


class FieldValidationException(Exception):
    title: str
    detail: str


PERMISSIONS = permissions.AllPerm(
    [
        permissions.Perm("wato.rulesets"),
        permissions.Optional(permissions.Perm("wato.all_folders")),
    ]
)

RW_PERMISSIONS = permissions.AllPerm(
    [
        permissions.Perm("wato.edit"),
        *PERMISSIONS.perms,
    ]
)


# NOTE: This is a dataclass and no namedtuple because it needs to be mutable. See `move_rule_to`
@dataclasses.dataclass
class RuleEntry:
    rule: Rule
    ruleset: Ruleset
    all_rulesets: AllRulesets
    # NOTE: Can't be called "index", because mypy doesn't like that. Duh.
    index_nr: int
    folder: Folder


def _validate_rule_move(lhs: RuleEntry, rhs: RuleEntry) -> None:
    if lhs.ruleset.name != rhs.ruleset.name:
        raise RestAPIRequestDataValidationException(
            title="Invalid rule move.", detail="The two rules are not in the same ruleset."
        )
    if lhs.rule.id == rhs.rule.id:
        raise RestAPIRequestDataValidationException(
            title="Invalid rule move", detail="You cannot move a rule before/after itself."
        )


@Endpoint(
    constructors.object_action_href("rule", "{rule_id}", "move"),
    "cmk/move",
    method="post",
    etag="input",
    path_params=[RULE_ID],
    request_schema=MoveRuleTo,
    response_schema=RuleObject,
    permissions_required=RW_PERMISSIONS,
)
def move_rule_to(param: typing.Mapping[str, typing.Any]) -> http.Response:
    """Move a rule to a specific location"""
    user.need_permission("wato.edit")
    user.need_permission("wato.rulesets")
    rule_id = param["rule_id"]

    body = param["body"]
    position = body["position"]

    source_entry = _get_rule_by_id(rule_id)

    all_rulesets = source_entry.all_rulesets

    index: int
    dest_folder: Folder
    match position:
        case "top_of_folder":
            dest_folder = body["folder"]
            index = Ruleset.TOP
        case "bottom_of_folder":
            dest_folder = body["folder"]
            index = Ruleset.BOTTOM
        case "before_specific_rule":
            dest_entry = _get_rule_by_id(body["rule_id"], all_rulesets=all_rulesets)
            _validate_rule_move(source_entry, dest_entry)
            index = dest_entry.index_nr
            dest_folder = dest_entry.folder
        case "after_specific_rule":
            dest_entry = _get_rule_by_id(body["rule_id"], all_rulesets=all_rulesets)
            _validate_rule_move(source_entry, dest_entry)
            dest_folder = dest_entry.folder
            index = dest_entry.index_nr + 1
        case _:
            return problem(
                status=400,
                title="Invalid position",
                detail=f"Position {position!r} is not a valid position.",
            )

    dest_folder.permissions.need_permission("write")
    source_entry.ruleset.move_to_folder(source_entry.rule, dest_folder, index)
    source_entry.folder = dest_folder
    all_rulesets.save()
    affected_sites = source_entry.folder.all_site_ids()

    if dest_folder != source_entry.folder:
        affected_sites.extend(dest_folder.all_site_ids())

    add_change(
        "edit-rule",
        _l('Changed properties of rule "%s", moved from folder "%s" to top of folder "%s"')
        % (source_entry.rule.id, source_entry.folder.title(), dest_folder.title()),
        sites=list(set(affected_sites)),
        object_ref=source_entry.rule.object_ref(),
    )

    return serve_json(_serialize_rule(source_entry))


@Endpoint(
    constructors.collection_href("rule"),
    "cmk/create",
    method="post",
    etag="output",
    request_schema=InputRuleObject,
    response_schema=RuleObject,
    permissions_required=RW_PERMISSIONS,
)
def create_rule(param):
    """Create rule"""
    user.need_permission("wato.edit")
    user.need_permission("wato.rulesets")
    body = param["body"]
    value = body["value_raw"]
    ruleset_name = body["ruleset"]

    folder: Folder = body["folder"]
    folder.permissions.need_permission("write")

    rulesets = FolderRulesets.load_folder_rulesets(folder)
    ruleset = _retrieve_from_rulesets(rulesets, ruleset_name)

    try:
        _validate_value(ruleset, value)

    except FieldValidationException as exc:
        return problem(
            status=400,
            detail=exc.detail,
            title=exc.title,
        )

    rule = _create_rule(folder, ruleset, body["conditions"], body["properties"], value, gen_id())

    index = ruleset.append_rule(folder, rule)
    rulesets.save_folder()
    # TODO Duplicated code is in pages/rulesets.py:2670-
    # TODO Move to
    add_change(
        "new-rule",
        _l('Created new rule #%d in ruleset "%s" in folder "%s"')
        % (index, ruleset.title(), folder.alias_path()),
        sites=folder.all_site_ids(),
        diff_text=make_diff_text({}, rule.to_log()),
        object_ref=rule.object_ref(),
    )
    rule_entry = _get_rule_by_id(rule.id)
    return serve_json(_serialize_rule(rule_entry))


@Endpoint(
    constructors.collection_href(domain_type="rule"),
    ".../collection",
    method="get",
    response_schema=RuleCollection,
    permissions_required=PERMISSIONS,
    query_params=[RuleSearchOptions],
)
def list_rules(param):
    """List rules"""
    user.need_permission("wato.rulesets")
    all_rulesets = AllRulesets.load_all_rulesets()
    ruleset_name = param["ruleset_name"]

    ruleset = _retrieve_from_rulesets(all_rulesets, ruleset_name)

    result = []
    for folder, index, rule in ruleset.get_rules():
        result.append(
            _serialize_rule(
                RuleEntry(
                    rule=rule,
                    ruleset=rule.ruleset,
                    folder=folder,
                    index_nr=index,
                    all_rulesets=all_rulesets,
                )
            )
        )

    return serve_json(
        constructors.collection_object(
            domain_type="rule",
            value=result,
            extensions={
                "found_rules": len(result),
            },
        )
    )


@Endpoint(
    constructors.object_href(domain_type="rule", obj_id="{rule_id}"),
    "cmk/show",
    method="get",
    response_schema=RuleObject,
    path_params=[RULE_ID],
    permissions_required=PERMISSIONS,
)
def show_rule(param):
    """Show a rule"""
    user.need_permission("wato.rulesets")
    rule_entry = _get_rule_by_id(param["rule_id"])
    return serve_json(_serialize_rule(rule_entry))


def _get_rule_by_id(rule_uuid: str, all_rulesets=None) -> RuleEntry:  # type: ignore[no-untyped-def]
    if all_rulesets is None:
        all_rulesets = AllRulesets.load_all_rulesets()

    for ruleset in visible_rulesets(all_rulesets.get_rulesets()).values():
        folder: Folder
        index: int
        rule: Rule
        for folder, index, rule in ruleset.get_rules():
            if rule.id == rule_uuid:
                return RuleEntry(
                    index_nr=index,
                    rule=rule,
                    folder=folder,
                    ruleset=ruleset,
                    all_rulesets=all_rulesets,
                )

    raise ProblemException(
        status=404,
        title="Unknown rule.",
        detail=f"Rule with UUID '{rule_uuid}' was not found.",
    )


@Endpoint(
    constructors.object_href(domain_type="rule", obj_id="{rule_id}"),
    ".../delete",
    method="delete",
    path_params=[RULE_ID],
    output_empty=True,
    status_descriptions={
        204: "Rule was deleted successfully.",
        404: "The rule to be deleted was not found.",
    },
    additional_status_codes=[
        204,
        404,
    ],
    permissions_required=RW_PERMISSIONS,
)
def delete_rule(param):
    """Delete a rule"""
    user.need_permission("wato.edit")
    user.need_permission("wato.rulesets")
    rule_id = param["rule_id"]
    rule: Rule
    all_rulesets = AllRulesets.load_all_rulesets()

    found = False
    for ruleset in visible_rulesets(all_rulesets.get_rulesets()).values():
        for _folder, _index, rule in ruleset.get_rules():
            if rule.id == rule_id:
                ruleset.delete_rule(rule)
                all_rulesets.save()
                found = True
    if found:
        return http.Response(status=204)

    return problem(
        status=404,
        title="Rule not found.",
        detail=f"The rule with ID {rule_id!r} could not be found.",
    )


@Endpoint(
    constructors.object_href(domain_type="rule", obj_id="{rule_id}"),
    ".../update",
    method="put",
    etag="both",
    path_params=[RULE_ID],
    request_schema=UpdateRuleObject,
    response_schema=RuleObject,
    permissions_required=RW_PERMISSIONS,
)
def edit_rule(param):
    """Modify a rule"""
    user.need_permission("wato.edit")
    user.need_permission("wato.rulesets")
    body = param["body"]
    value = body["value_raw"]
    rule_entry = _get_rule_by_id(param["rule_id"])

    folder: Folder = rule_entry.folder
    folder.permissions.need_permission("write")

    ruleset = rule_entry.ruleset
    rulesets = rule_entry.all_rulesets
    current_rule = rule_entry.rule

    try:
        _validate_value(ruleset, value)

    except FieldValidationException as exc:
        return problem(
            status=400,
            detail=exc.detail,
            title=exc.title,
        )

    new_rule = _create_rule(
        folder, ruleset, body["conditions"], body["properties"], value, param["rule_id"]
    )

    ruleset.edit_rule(current_rule, new_rule)
    rulesets.save_folder(folder)

    new_rule_entry = _get_rule_by_id(param["rule_id"])
    return serve_json(_serialize_rule(new_rule_entry))


def _validate_value(ruleset: Ruleset, value: typing.Any) -> None:
    try:
        ruleset.valuespec().validate_value(value, "")

    except exceptions.MKUserError as exc:
        if exc.varname is None:
            title = "A field has a problem"
        else:
            field_name = strip_tags(exc.varname.replace("_p_", ""))
            title = f"Problem in (sub-)field {field_name!r}"

        exception = FieldValidationException()
        exception.title = title
        exception.detail = strip_tags(exc.message)
        raise exception


def _create_rule(
    folder: Folder,
    ruleset: Ruleset,
    conditions: dict[str, typing.Any],
    properties: RuleOptionsSpec,
    value: typing.Any,
    rule_id: str = gen_id(),
) -> Rule:
    rule = Rule(
        rule_id,
        folder,
        ruleset,
        RuleConditions(
            host_folder=folder.path(),
            host_tags=conditions.get("host_tags"),
            host_labels=conditions.get("host_labels"),
            host_name=conditions.get("host_name"),
            service_description=conditions.get("service_description"),
            service_labels=conditions.get("service_labels"),
        ),
        RuleOptions.from_config(properties),
        value,
    )

    return rule


def _retrieve_from_rulesets(rulesets: RulesetCollection, ruleset_name: str) -> Ruleset:
    ruleset_exception = ProblemException(
        status=400,
        title="Unknown ruleset.",
        detail=f"The ruleset of name {ruleset_name!r} is not known.",
    )
    try:
        ruleset = rulesets.get(ruleset_name)
    except KeyError:
        raise ruleset_exception

    if not visible_ruleset(ruleset.rulespec.name):
        raise ruleset_exception

    return ruleset


def _serialize_rule(rule_entry: RuleEntry) -> DomainObject:
    rule = rule_entry.rule
    return constructors.domain_object(
        domain_type="rule",
        editable=False,
        identifier=rule.id,
        title=rule.description(),
        extensions={
            "ruleset": rule.ruleset.name,
            "folder": "/" + rule_entry.folder.path(),
            "folder_index": rule_entry.index_nr,
            "properties": rule.rule_options.to_config(),
            "value_raw": repr(rule.ruleset.valuespec().mask(rule.value)),
            "conditions": denilled(
                {
                    "host_name": rule.conditions.host_name,
                    "host_tags": rule.conditions.host_tags,
                    "host_labels": rule.conditions.host_labels,
                    "service_description": rule.conditions.service_description,
                    "service_labels": rule.conditions.service_labels,
                }
            ),
        },
    )
