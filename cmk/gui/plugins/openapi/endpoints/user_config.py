#!/usr/bin/env python3
# Copyright (C) 2020 tribe29 GmbH - License: GNU General Public License v2
# This file is part of Checkmk (https://checkmk.com). It is subject to the terms and
# conditions defined in the file COPYING, which is part of this source code package.
"""Users"""
import datetime as dt
import time
from collections.abc import Mapping
from typing import Any, Literal, TypedDict

from cmk.utils.crypto.password import Password
from cmk.utils.type_defs import UserId

import cmk.gui.plugins.userdb.utils as userdb_utils
from cmk.gui import userdb
from cmk.gui.exceptions import MKUserError
from cmk.gui.http import Response
from cmk.gui.logged_in import user
from cmk.gui.plugins.openapi.endpoints.utils import complement_customer, update_customer_info
from cmk.gui.plugins.openapi.restful_objects import (
    constructors,
    Endpoint,
    permissions,
    request_schemas,
    response_schemas,
)
from cmk.gui.plugins.openapi.restful_objects.parameters import USERNAME
from cmk.gui.plugins.openapi.utils import problem, ProblemException, serve_json
from cmk.gui.type_defs import UserSpec
from cmk.gui.userdb import htpasswd
from cmk.gui.watolib.custom_attributes import load_custom_attrs_from_mk_file
from cmk.gui.watolib.users import delete_users, edit_users, verify_password_policy

TIMESTAMP_RANGE = tuple[float, float]


class ApiInterfaceAttributes(TypedDict, total=False):
    interface_theme: Literal["default", "dark", "light"]
    sidebar_position: Literal["left", "right"]
    navigation_bar_icons: Literal["show", "hide"]
    mega_menu_icons: Literal["topic", "entry"]
    show_mode: Literal["default", "default_show_less", "default_show_more", "enforce_show_more"]


class InternalInterfaceAttributes(TypedDict, total=False):
    ui_theme: Literal["modern-dark", "facelift"] | None
    ui_sidebar_position: Literal["left"] | None
    nav_hide_icons_title: Literal["hide"] | None
    icons_per_item: Literal["entry"] | None
    show_mode: Literal["default_show_less", "default_show_more", "enforce_show_more"] | None


PERMISSIONS = permissions.Perm("wato.users")

RW_PERMISSIONS = permissions.AllPerm(
    [
        permissions.Perm("wato.edit"),
        PERMISSIONS,
    ]
)


@Endpoint(
    constructors.object_href("user_config", "{username}"),
    "cmk/show",
    method="get",
    path_params=[USERNAME],
    etag="output",
    response_schema=response_schemas.UserObject,
    permissions_required=PERMISSIONS,
)
def show_user(params: Mapping[str, Any]) -> Response:
    """Show a user"""
    user.need_permission("wato.users")
    username = params["username"]
    try:
        return serve_user(username)
    except KeyError:
        return problem(
            404,
            f"User '{username}' is not known.",
            "The user you asked for is not known. Please check for eventual misspellings.",
        )


@Endpoint(
    constructors.collection_href("user_config"),
    ".../collection",
    method="get",
    response_schema=response_schemas.UserCollection,
    permissions_required=PERMISSIONS,
)
def list_users(params: Mapping[str, Any]) -> Response:
    """Show all users"""
    user.need_permission("wato.users")
    users = []
    for user_id, attrs in userdb.load_users(False).items():
        user_attributes = _internal_to_api_format(attrs)
        users.append(serialize_user(user_id, complement_customer(user_attributes)))

    return serve_json(constructors.collection_object(domain_type="user_config", value=users))


@Endpoint(
    constructors.collection_href("user_config"),
    "cmk/create",
    method="post",
    etag="output",
    request_schema=request_schemas.CreateUser,
    response_schema=response_schemas.UserObject,
    permissions_required=permissions.AllPerm(
        [
            *RW_PERMISSIONS.perms,
            permissions.Optional(permissions.Perm("wato.groups")),
        ]
    ),
)
def create_user(params: Mapping[str, Any]) -> Response:
    """Create a user

    You can pass custom attributes you defined directly in the top level JSON object of the request.
    """
    api_attrs = params["body"]
    username = api_attrs["username"]

    # The interface options must be set for a new user, but we restrict the setting through the API
    internal_attrs: UserSpec = {
        "start_url": None,
        "force_authuser": False,
    }

    internal_attrs = _api_to_internal_format(internal_attrs, api_attrs, new_user=True)
    edit_users(
        {
            username: {
                "attributes": internal_attrs,
                "is_new_user": True,
            }
        }
    )
    return serve_user(username)


@Endpoint(
    constructors.object_href("user_config", "{username}"),
    ".../delete",
    method="delete",
    path_params=[USERNAME],
    output_empty=True,
    permissions_required=RW_PERMISSIONS,
)
def delete_user(params: Mapping[str, Any]) -> Response:
    """Delete a user"""
    username = params["username"]
    try:
        delete_users([username])
    except MKUserError:
        return problem(
            status=404,
            title=f'User "{username}" is not known.',
            detail="The user to delete does not exist. Please check for eventual misspellings.",
        )
    return Response(status=204)


@Endpoint(
    constructors.object_href("user_config", "{username}"),
    ".../update",
    method="put",
    path_params=[USERNAME],
    etag="both",
    request_schema=request_schemas.UpdateUser,
    response_schema=response_schemas.UserObject,
    permissions_required=RW_PERMISSIONS,
)
def edit_user(params: Mapping[str, Any]) -> Response:
    """Edit a user"""
    # last_pw_change & serial must be changed manually if edit happens
    username = params["username"]
    api_attrs = params["body"]

    try:
        internal_attrs = _api_to_internal_format(_load_user(username), api_attrs)
    except KeyError:
        return problem(
            status=404,
            title=f'User "{username}" is not known.',
            detail="The user to edit does not exist. Please check for eventual misspellings.",
        )

    edit_users(
        {
            username: {
                "attributes": internal_attrs,
                "is_new_user": False,
            }
        }
    )
    return serve_user(username)


def serve_user(user_id):
    user_attributes_internal = _load_user(user_id)
    user_attributes = _internal_to_api_format(user_attributes_internal)
    response = serve_json(serialize_user(user_id, complement_customer(user_attributes)))
    response.headers.add("ETag", constructors.etag_of_dict(user_attributes).to_header())
    return response


def serialize_user(user_id, attributes):
    return constructors.domain_object(
        domain_type="user_config",
        identifier=user_id,
        title=attributes["fullname"],
        extensions=attributes,
    )


def _api_to_internal_format(internal_attrs, api_configurations, new_user=False):
    for attr, value in api_configurations.items():
        if attr in (
            "username",
            "customer",
            "contact_options",
            "auth_option",
            "authorized_sites",
            "idle_timeout",
            "disable_notifications",
            "interface_options",
        ):
            continue
        internal_attrs[attr] = value

    if "customer" in api_configurations:
        internal_attrs = update_customer_info(
            internal_attrs, api_configurations["customer"], remove_provider=True
        )

    if (authorized_sites := api_configurations.get("authorized_sites")) is not None:
        if authorized_sites and "all" not in authorized_sites:
            internal_attrs["authorized_sites"] = authorized_sites
        # Update with all
        elif "all" in authorized_sites and "authorized_sites" in internal_attrs:
            del internal_attrs["authorized_sites"]

    internal_attrs.update(
        _interface_options_to_internal_format(api_configurations.get("interface_options", {}))
    )
    internal_attrs.update(
        _contact_options_to_internal_format(
            api_configurations.get("contact_options"), internal_attrs.get("email")
        )
    )
    internal_attrs = _update_auth_options(
        internal_attrs, api_configurations["auth_option"], new_user=new_user
    )
    internal_attrs = _update_notification_options(
        internal_attrs, api_configurations.get("disable_notifications")
    )
    internal_attrs = _update_idle_options(internal_attrs, api_configurations.get("idle_timeout"))
    return internal_attrs


def _internal_to_api_format(  # pylint: disable=too-many-branches
    internal_attrs: UserSpec,
) -> dict[str, Any]:
    api_attrs: dict[str, Any] = {}
    api_attrs.update(_idle_options_to_api_format(internal_attrs))
    api_attrs["auth_option"] = _auth_options_to_api_format(internal_attrs)
    api_attrs.update(_notification_options_to_api_format(internal_attrs))

    iia = InternalInterfaceAttributes()
    if "ui_theme" in internal_attrs:
        iia["ui_theme"] = internal_attrs["ui_theme"]
    if "ui_sidebar_position" in internal_attrs:
        iia["ui_sidebar_position"] = internal_attrs["ui_sidebar_position"]
    if "nav_hide_icons_title" in internal_attrs:
        iia["nav_hide_icons_title"] = internal_attrs["nav_hide_icons_title"]
    if "icons_per_item" in internal_attrs:
        iia["icons_per_item"] = internal_attrs["icons_per_item"]
    if "show_mode" in internal_attrs:
        iia["show_mode"] = internal_attrs["show_mode"]  # type: ignore[typeddict-item]
    if interface_options := _interface_options_to_api_format(iia):
        api_attrs["interface_options"] = interface_options

    if "email" in internal_attrs:
        api_attrs.update(_contact_options_to_api_format(internal_attrs))

    if "locked" in internal_attrs:
        api_attrs["disable_login"] = internal_attrs["locked"]

    if "alias" in internal_attrs:
        api_attrs["fullname"] = internal_attrs["alias"]

    if "pager" in internal_attrs:
        api_attrs["pager_address"] = internal_attrs["pager"]

    api_attrs.update(
        {
            k: v
            for k, v in internal_attrs.items()
            if k
            in (
                "roles",
                "contactgroups",
                "language",
                "customer",
            )
        }
    )
    custom_attrs = load_custom_attrs_from_mk_file(lock=False)["user"]
    for attr in custom_attrs:
        if (name := attr["name"]) in internal_attrs:
            # monkeypatch a typed dict, what can go wrong
            api_attrs[name] = internal_attrs[name]  # type: ignore[literal-required]
    return api_attrs


def _idle_options_to_api_format(internal_attributes: UserSpec) -> dict[str, dict[str, Any]]:
    if "idle_timeout" in internal_attributes:
        idle_option = internal_attributes["idle_timeout"]
        if idle_option:
            idle_details = {"option": "individual", "duration": idle_option}
        else:  # False
            idle_details = {"option": "disable"}
    else:
        idle_details = {"option": "global"}

    return {"idle_timeout": idle_details}


class APIAuthOption(TypedDict, total=False):
    # TODO: this should be adapted with the introduction of an enum
    auth_type: Literal["automation", "password", "saml2", "ldap"]
    enforce_password_change: bool


def _auth_options_to_api_format(internal_attributes: UserSpec) -> APIAuthOption:
    result: APIAuthOption = {}

    # TODO: the default ConnectorType.HTPASSWD is currently a bug #CMK-12723 but not wrong
    connector = internal_attributes.get("connector", userdb_utils.ConnectorType.HTPASSWD)
    if connector == userdb_utils.ConnectorType.HTPASSWD:
        if "automation_secret" in internal_attributes:
            result["auth_type"] = "automation"
        elif "password" in internal_attributes:
            result["auth_type"] = "password"
            if (
                "enforce_pw_change" in internal_attributes
                and (enforce_password_change := internal_attributes["enforce_pw_change"])
                is not None
            ):
                result["enforce_password_change"] = enforce_password_change
        return result

    for connection in userdb_utils.load_connection_config():
        if connection["id"] == connector:
            result["auth_type"] = connection["type"]

    return result


def _contact_options_to_api_format(internal_attributes):
    return {
        "contact_options": {
            "email": internal_attributes["email"],
            "fallback_contact": internal_attributes.get("fallback_contact", False),
        }
    }


def _notification_options_to_api_format(internal_attributes):
    internal_notification_options = internal_attributes.get("disable_notifications")
    if not internal_notification_options:
        return {"disable_notifications": {}}

    options = {}
    if "timerange" in internal_notification_options:
        timerange = internal_notification_options["timerange"]
        options.update({"timerange": {"start_time": timerange[0], "end_time": timerange[1]}})

    if "disable" in internal_notification_options:
        options["disable"] = internal_notification_options["disable"]

    return {"disable_notifications": options}


class ContactOptions(TypedDict, total=False):
    email: str
    fallback_contact: bool


def _contact_options_to_internal_format(  # type: ignore[no-untyped-def]
    contact_options: ContactOptions, current_email: str | None = None
):
    updated_details: dict[str, str | bool] = {}
    if not contact_options:
        return updated_details

    if "email" in contact_options:
        current_email = contact_options["email"]
        updated_details["email"] = current_email

    if "fallback_contact" in contact_options:
        fallback = contact_options["fallback_contact"]
        if fallback:
            if not current_email:
                raise ProblemException(
                    status=400,
                    title="Fallback contact option requires email",
                    detail="Fallback contact option requires configuration of a mail for the user",
                )
            fallback_option = True
        else:
            fallback_option = False
        updated_details["fallback_contact"] = fallback_option

    return updated_details


class AuthOptions(TypedDict, total=False):
    auth_type: Literal["remove", "automation", "password"]
    password: str
    secret: str
    enforce_password_change: bool


def _update_auth_options(
    internal_attrs: dict[str, int | str | bool], auth_options: AuthOptions, new_user: bool = False
) -> dict[str, int | str | bool]:
    """Update the internal attributes with the authentication options (used for create and update)

    Notes:
        * the REST API currently only allows creating users with htpasswd connector (not LDAP
        or SAML2)
            * the connector must also be set even if there is no authentication specified
    """
    if not auth_options:
        if new_user:
            internal_attrs["connector"] = userdb_utils.ConnectorType.HTPASSWD
        return internal_attrs

    if auth_options.get("auth_type") == "remove":
        internal_attrs.pop("automation_secret", None)
        internal_attrs.pop("password", None)
        internal_attrs["serial"] = 1
    else:
        internal_auth_attrs = _auth_options_to_internal_format(auth_options)
        if new_user and "password" not in internal_auth_attrs:
            # "password" (the password hash) is set for both automation users and regular users,
            # although automation users don't really use it yet (but they should, eventually).
            raise MKUserError(None, "No authentication details provided for new user")

        if internal_auth_attrs:
            if "automation_secret" not in internal_auth_attrs:  # new password
                internal_attrs.pop("automation_secret", None)
            # Note: Changing from password to automation secret leaves enforce_pw_change, although
            #       it will be ignored for automation users.
            internal_attrs.update(internal_auth_attrs)

            if internal_auth_attrs.get("enforce_password_change"):
                internal_attrs["serial"] = 1

            if "password" in auth_options or "secret" in auth_options:
                internal_attrs["serial"] = 1

        internal_attrs["connector"] = userdb_utils.ConnectorType.HTPASSWD
    return internal_attrs


def _auth_options_to_internal_format(auth_details: AuthOptions) -> dict[str, int | str | bool]:
    """Format the authentication information to be Checkmk compatible

    Args:
        auth_details:
            user provided authentication details

    Returns:
        formatted authentication details for Checkmk user_attrs

    Examples:

    Setting a new automation secret:

        >>> _auth_options_to_internal_format(
        ...     {"auth_type": "automation", "secret": "TNBJCkwane3$cfn0XLf6p6a"}
        ... )  # doctest:+ELLIPSIS
        {'password': ..., 'automation_secret': 'TNBJCkwane3$cfn0XLf6p6a', 'last_pw_change': ...}

    Enforcing password change without changing the password:

        >>> _auth_options_to_internal_format(
        ...     {"auth_type": "password", "enforce_password_change": True}
        ... )
        {'enforce_pw_change': True}

    Empty password is not allowed and passwords result in MKUserErrors:

        >>> _auth_options_to_internal_format(
        ...     {"auth_type": "password", "enforce_password_change": True, "password": ""}
        ... )
        Traceback (most recent call last):
        ...
        cmk.gui.exceptions.MKUserError: Password must not be empty

        >>> _auth_options_to_internal_format(
        ...     {"auth_type": "password", "enforce_password_change": True, "password": "\\0"}
        ... )
        Traceback (most recent call last):
        ...
        cmk.gui.exceptions.MKUserError: Password must not contain null bytes
    """
    internal_options: dict[str, str | bool | int] = {}
    if not auth_details:
        return internal_options

    auth_type = auth_details["auth_type"]
    assert auth_type in ["automation", "password"]  # assuming remove was handled above...

    password_field: Literal["secret", "password"] = (
        "secret" if auth_type == "automation" else "password"
    )
    if password_field in auth_details:
        try:
            password = Password(auth_details[password_field])
        except ValueError as e:
            raise MKUserError(password_field, str(e))

        # Re-using the htpasswd wrapper for hash_password here, so we get MKUserErrors.
        internal_options["password"] = htpasswd.hash_password(password)

        if auth_type == "password":
            verify_password_policy(password)

        if auth_type == "automation":
            internal_options["automation_secret"] = password.raw

        # In contrast to enforce_pw_change, the maximum password age is enforced for automation
        # users as well. So set this for both kinds of users.
        internal_options["last_pw_change"] = int(time.time())

    if "enforce_password_change" in auth_details:
        # Note that enforce_pw_change cannot be set for automation users. We rely on the schema to
        # ensure that.
        internal_options["enforce_pw_change"] = auth_details["enforce_password_change"]

    return internal_options


class IdleDetails(TypedDict, total=False):
    option: Literal["disable", "individual", "global"]
    duration: int


def _update_idle_options(internal_attrs, idle_details: IdleDetails):  # type: ignore[no-untyped-def]
    if not idle_details:
        return internal_attrs

    idle_option = idle_details["option"]
    if idle_option == "disable":
        internal_attrs["idle_timeout"] = False
    elif idle_option == "individual":
        internal_attrs["idle_timeout"] = idle_details["duration"]
    else:  # global configuration, only for update
        internal_attrs.pop("idle_timeout", None)
    return internal_attrs


def _interface_options_to_internal_format(
    api_interface_options: ApiInterfaceAttributes,
) -> InternalInterfaceAttributes:
    internal_inteface_options = InternalInterfaceAttributes()
    if theme := api_interface_options.get("interface_theme"):
        internal_inteface_options["ui_theme"] = {
            "default": None,
            "dark": "modern-dark",
            "light": "facelift",
        }[theme]
    if sidebar_position := api_interface_options.get("sidebar_position"):
        internal_inteface_options["ui_sidebar_position"] = {"right": None, "left": "left"}[
            sidebar_position
        ]
    if show_icon_titles := api_interface_options.get("navigation_bar_icons"):
        internal_inteface_options["nav_hide_icons_title"] = {"show": None, "hide": "hide"}[
            show_icon_titles
        ]
    if mega_menu_icons := api_interface_options.get("mega_menu_icons"):
        internal_inteface_options["icons_per_item"] = {"topic": None, "entry": "entry"}[
            mega_menu_icons
        ]
    if show_mode := api_interface_options.get("show_mode"):
        internal_inteface_options["show_mode"] = {
            "default": None,
            "default_show_less": "default_show_less",
            "default_show_more": "default_show_more",
            "enforce_show_more": "enforce_show_more",
        }[show_mode]
    return internal_inteface_options


def _interface_options_to_api_format(
    internal_interface_options: InternalInterfaceAttributes,
) -> ApiInterfaceAttributes:
    attributes = ApiInterfaceAttributes()
    if "ui_sidebar_position" not in internal_interface_options:
        attributes["sidebar_position"] = "right"
    else:
        attributes["sidebar_position"] = "left"

    if "nav_hide_icons_title" in internal_interface_options:
        attributes["navigation_bar_icons"] = (
            "show" if internal_interface_options["nav_hide_icons_title"] is None else "hide"
        )

    if "icons_per_item" in internal_interface_options:
        attributes["mega_menu_icons"] = (
            "topic" if internal_interface_options["icons_per_item"] is None else "entry"
        )

    if "show_mode" in internal_interface_options:
        attributes["show_mode"] = (
            "default"
            if internal_interface_options["show_mode"] is None
            else internal_interface_options["show_mode"]
        )

    if "ui_theme" not in internal_interface_options:
        attributes["interface_theme"] = "default"
    elif internal_interface_options["ui_theme"] == "modern-dark":
        attributes["interface_theme"] = "dark"
    elif internal_interface_options["ui_theme"] == "facelift":
        attributes["interface_theme"] = "light"
    else:
        # TODO: What should *really* be done in case of None?
        pass

    return attributes


def _load_user(username: UserId) -> UserSpec:
    """return UserSpec for username

    CAUTION: the UserSpec contains sensitive data like password hashes"""

    # TODO: verify additional edge cases
    return userdb.load_users(lock=False)[username]


class TimeRange(TypedDict):
    start_time: dt.datetime
    end_time: dt.datetime


class NotificationDetails(TypedDict, total=False):
    timerange: TimeRange
    disable: bool


def _update_notification_options(  # type: ignore[no-untyped-def]
    internal_attrs, notification_options: NotificationDetails
):
    internal_attrs["disable_notifications"] = _notification_options_to_internal_format(
        internal_attrs.get("disable_notifications", {}), notification_options
    )
    return internal_attrs


def _notification_options_to_internal_format(
    notification_internal: dict[str, bool | TIMESTAMP_RANGE],
    notification_api_details: NotificationDetails,
) -> dict[str, bool | TIMESTAMP_RANGE]:
    """Format disable notifications information to be Checkmk compatible

    Args:
        notification_api_details:
            user provided notifications details

    Returns:
        formatted disable notifications details for Checkmk user_attrs

    Example:
        >>> _notification_options_to_internal_format(
        ... {},
        ... {"timerange":{
        ... 'start_time': dt.datetime.strptime("2020-01-01T13:00:00Z", "%Y-%m-%dT%H:%M:%SZ"),
        ... 'end_time': dt.datetime.strptime("2020-01-01T14:00:00Z", "%Y-%m-%dT%H:%M:%SZ")
        ... }})
        {'timerange': (1577883600.0, 1577887200.0)}
    """
    if not notification_api_details:
        return notification_internal

    if "timerange" in notification_api_details:
        notification_internal["timerange"] = _time_stamp_range(
            notification_api_details["timerange"]
        )

    if "disable" in notification_api_details:
        if notification_api_details["disable"]:
            notification_internal["disable"] = True
        else:
            notification_internal.pop("disable", None)

    return notification_internal


def _time_stamp_range(datetime_range: TimeRange) -> TIMESTAMP_RANGE:
    def timestamp(date_time):
        return dt.datetime.timestamp(date_time.replace(tzinfo=dt.timezone.utc))

    return timestamp(datetime_range["start_time"]), timestamp(datetime_range["end_time"])
