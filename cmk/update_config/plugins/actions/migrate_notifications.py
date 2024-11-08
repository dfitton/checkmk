#!/usr/bin/env python3
# Copyright (C) 2024 Checkmk GmbH - License: GNU General Public License v2
# This file is part of Checkmk (https://checkmk.com). It is subject to the terms and
# conditions defined in the file COPYING, which is part of this source code package.

from logging import Logger
from pathlib import Path
from typing import cast

from cmk.utils import tty
from cmk.utils.notify_types import (
    EventRule,
    NotificationParameterGeneralInfos,
    NotificationParameterItem,
    NotificationParameterMethod,
    NotificationParameterSpecs,
)
from cmk.utils.paths import check_mk_config_dir, omd_root

from cmk.gui.watolib import sample_config
from cmk.gui.watolib.notifications import (
    NotificationParameterConfigFile,
    NotificationRuleConfigFile,
)

from cmk.update_config.registry import update_action_registry, UpdateAction


class MigrateNotifications(UpdateAction):
    def __init__(self, name: str, title: str, sort_index: int) -> None:
        super().__init__(name=name, title=title, sort_index=sort_index)
        self._notifications_mk_path: Path = Path(check_mk_config_dir, "wato/notifications.mk")
        self._notifications_mk_backup_path: Path = omd_root / "notifications_backup.mk"

    def __call__(self, logger: Logger) -> None:
        if all(
            isinstance(event_rule["notify_plugin"][1], str)
            for event_rule in NotificationRuleConfigFile().load_for_reading()
        ):
            logger.debug("       Already migrated")
            return

        logger.debug("       Start backup of existing notification configuration.")
        self._backup_notification_config(logger)
        logger.debug("       Finished backup of existing notification configuration.")

        parameters_per_method: NotificationParameterSpecs = {}
        updated_notification_rules: list[EventRule] = []
        for nr, rule in enumerate(NotificationRuleConfigFile().load_for_reading()):
            method, parameter = rule["notify_plugin"]

            if parameter is None:
                rule["notify_plugin"] = (method, parameter)
                updated_notification_rules.append(rule)
                continue

            parameters_per_method.setdefault(
                NotificationParameterMethod(method),
                {},
            )

            parameter_id = [
                param_id
                for param_id, params in parameters_per_method[method].items()
                if params["parameter_properties"] == parameter  # type: ignore[comparison-overlap]
            ]

            if not parameter_id:
                parameter_id = [sample_config.new_notification_parameter_id()]

                # Call with the following parameter...
                if isinstance(parameter, list):
                    parameter = {"params": parameter}

                parameters_per_method[method].update(
                    {
                        parameter_id[0]: NotificationParameterItem(
                            general=NotificationParameterGeneralInfos(
                                description="Migrated from notification rule #%d" % nr,
                                comment="Auto migrated on update",
                                docu_url="",
                            ),
                            parameter_properties=cast(dict, parameter),
                        )
                    }
                )

            rule["notify_plugin"] = (method, parameter_id[0])
            updated_notification_rules.append(rule)

        NotificationParameterConfigFile().save(parameters_per_method)
        logger.debug("       Saved migrated notification parameters")
        NotificationRuleConfigFile().save(updated_notification_rules)
        logger.debug("       Saved migrated notification rules")

    def _backup_notification_config(self, logger: Logger) -> None:
        self._notifications_mk_backup_path.write_text(self._notifications_mk_path.read_text())
        logger.info(
            f"{tty.yellow}       Wrote notification configuration backup to\n"
            f"       {str(self._notifications_mk_backup_path)}.\n\n"
            "       Please check if the notification pages in the GUI work as "
            "expected.\n       In case of problems you can copy the backup "
            "files back to \n"
            f"       {str(self._notifications_mk_path)}.\n"
            "       If everything works as expected you can remove the backup.\n"
        )


update_action_registry.register(
    MigrateNotifications(
        name="migrate_notifications",
        title="Migrate notifications",
        sort_index=50,
    )
)