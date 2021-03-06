from typing import Any

from grapl_analyzerlib.analyzer import Analyzer, OneOrMany
from grapl_analyzerlib.nodes.comparators import Not
from grapl_analyzerlib.prelude import ProcessQuery, ProcessView
from grapl_analyzerlib.execution import ExecutionHit


class SuspiciousSvchost(Analyzer):

    def get_queries(self) -> OneOrMany[ProcessQuery]:
        invalid_parents = [
            Not("services.exe"),
            Not("smss.exe"),
            Not("ngentask.exe"),
            Not("userinit.exe"),
            Not("GoogleUpdate.exe"),
            Not("conhost.exe"),
            Not("MpCmdRun.exe"),
        ]

        return (
            ProcessQuery()
            .with_process_name(eq=invalid_parents)
            .with_children(
                ProcessQuery().with_process_name(eq="svchost.exe")
            )
        )

    def on_response(self, response: ProcessView, output: Any):
        asset_id = response.get_asset().get_hostname()

        output.send(
            ExecutionHit(
                analyzer_name="Suspicious svchost",
                node_view=response,
                risk_score=75,
                lenses=asset_id
            )
        )


def get_descendents(process: ProcessView) -> None:
    children = process.get_children()
    for child in children:
        get_descendents(child)
