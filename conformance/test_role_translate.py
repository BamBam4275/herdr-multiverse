"""Unit coverage for the replay-time old->new role translation."""

from __future__ import annotations

import unittest

from role_translate import translate_config_toml, translate_expect_text, translate_role_argv


class TranslateConfigTomlTests(unittest.TestCase):
    def test_role_table_and_its_subtable_are_renamed(self):
        text = (
            "[roles.explorer]\nwrite_scope = \"none\"\n\n"
            "[roles.explorer.models]\ncodex = \"gpt-test\"\n"
        )
        self.assertEqual(
            translate_config_toml(text),
            "[roles.scout]\nwrite_scope = \"none\"\n\n"
            "[roles.scout.models]\ncodex = \"gpt-test\"\n",
        )

    def test_chain_entry_key_is_renamed_but_its_seat_values_are_not(self):
        text = "[projects.sandbox.chains]\nexplorer = [\"explorer-seat\"]\n"
        self.assertEqual(
            translate_config_toml(text),
            "[projects.sandbox.chains]\nscout = [\"explorer-seat\"]\n",
        )

    def test_account_names_and_free_text_values_survive_untouched(self):
        text = (
            "[accounts.explorer-seat]\nkind = \"codex\"\npool = \"codex:explorer\"\n"
        )
        self.assertEqual(translate_config_toml(text), text)

    def test_a_table_outside_chains_is_never_treated_as_a_chains_table(self):
        text = "[projects.sandbox]\nexplorer = \"not a chain entry\"\n"
        self.assertEqual(translate_config_toml(text), text)

    def test_council_is_left_unchanged(self):
        text = "[roles.council]\nwrite_scope = \"none\"\n"
        self.assertEqual(translate_config_toml(text), text)


class TranslateRoleArgvTests(unittest.TestCase):
    def test_dispatch_role_value_is_translated(self):
        argv = ["dispatch", "--dry-run", "--role", "fixer", "/proj", "fix it"]
        self.assertEqual(
            translate_role_argv(argv),
            ["dispatch", "--dry-run", "--role", "builder", "/proj", "fix it"],
        )

    def test_backlog_ask_role_value_is_translated(self):
        argv = ["backlog", "ask", "q", "--role", "fixer"]
        self.assertEqual(
            translate_role_argv(argv),
            ["backlog", "ask", "q", "--role", "builder"],
        )

    def test_backlog_add_role_value_is_translated(self):
        argv = ["backlog", "add", "title", "--role", "oracle"]
        self.assertEqual(
            translate_role_argv(argv),
            ["backlog", "add", "title", "--role", "seer"],
        )

    def test_argv_without_role_is_unchanged(self):
        argv = ["workers"]
        self.assertEqual(translate_role_argv(argv), argv)

    def test_council_role_already_current_is_left_alone(self):
        argv = ["council", "--role", "council"]
        self.assertEqual(translate_role_argv(argv), argv)


class TranslateExpectTextTests(unittest.TestCase):
    def test_card_role_line_is_translated(self):
        text = "  role        Explorer\n  column      Execute  (stage, not a role — spec §12)\n"
        self.assertEqual(
            translate_expect_text(text),
            "  role        Scout\n  column      Execute  (stage, not a role — spec §12)\n",
        )

    def test_would_log_role_field_is_translated_but_the_account_name_is_not(self):
        text = (
            "event=dispatch\ttask=<ID>\tproject=sandbox\trole=explorer\t"
            "account=explorer-seat\tpool=codex:test\toutcome=chain-primary"
        )
        self.assertEqual(
            translate_expect_text(text),
            "event=dispatch\ttask=<ID>\tproject=sandbox\trole=scout\t"
            "account=explorer-seat\tpool=codex:test\toutcome=chain-primary",
        )

    def test_chain_exhausted_refusal_is_translated(self):
        text = "\nevery account in the Explorer chain for 'sandbox' is unavailable:\n  - seat-a: x"
        self.assertEqual(
            translate_expect_text(text),
            "\nevery account in the Scout chain for 'sandbox' is unavailable:\n  - seat-a: x",
        )

    def test_degrade_once_fallback_policy_sentence_is_translated(self):
        text = (
            "\nevery account in the Fixer chain for 'sandbox' is unavailable:\n"
            "  - seat-a: outage a\n\n"
            "Fixer's fallback policy is degrade_once, which accepts only one step down."
        )
        self.assertEqual(
            translate_expect_text(text),
            "\nevery account in the Builder chain for 'sandbox' is unavailable:\n"
            "  - seat-a: outage a\n\n"
            "Builder's fallback policy is degrade_once, which accepts only one step down.",
        )

    def test_a_history_reads_old_role_word_is_left_alone(self):
        # `tasks`/`result` render an on-disk record verbatim forever — this
        # must never be rewritten, even though it is the same old word.
        text = "# Result — task <ID>\nRole: Designer\nStatus: done\n\nthe designer's finding"
        self.assertEqual(translate_expect_text(text), text)

    def test_a_history_role_column_is_left_alone(self):
        text = "<ID>      fixer      old-seat   recorded    done #3"
        self.assertEqual(translate_expect_text(text), text)

    def test_review_policy_common_noun_phrases_are_translated(self):
        text = (
            "After 2 REVISE round(s), stop asking for another review and "
            "escalate to the orchestrator instead.\n"
            "Timeout: 30 minute(s) — `multiverse sweep` flags a reviewer "
            "still running past this.\n"
            "Run tests: no — the orchestrator already ran the mutations\n"
            "reviewer seats:\n"
            "  (no project configures a reviewer chain)\n"
            "reviewer dispatch budget:\n"
        )
        self.assertEqual(
            translate_expect_text(text),
            "After 2 REVISE round(s), stop asking for another review and "
            "escalate to the nexus instead.\n"
            "Timeout: 30 minute(s) — `multiverse sweep` flags a critic "
            "still running past this.\n"
            "Run tests: no — the nexus already ran the mutations\n"
            "critic seats:\n"
            "  (no project configures a critic chain)\n"
            "critic dispatch budget:\n",
        )


class TranslateProseRolesTests(unittest.TestCase):
    def test_boxed_role_labels_keep_width_and_centering(self):
        from role_translate import ROLE_RENAME, translate_prose_roles

        def footer(role, width, short=""):
            label = f" {role} "
            tail = f" {short} " if short else ""
            rails = width - 2 - len(label) - len(tail)
            return "└" + "─" * (rails // 2) + label + "─" * (rails - rails // 2) + tail + "┘"

        for old, new in ROLE_RENAME.items():
            for width in (37, 38):
                for case in (str.lower, str.title, str.upper):
                    for short in ("", "sbx"):
                        with self.subTest(role=old, width=width, case=case, short=short):
                            original = footer(case(old), width, short)
                            self.assertEqual(translate_prose_roles(original),
                                             footer(case(new), width, short))

        original = "│ " + footer("fixer", 37) + " " + footer("explorer", 37) + " │\n"
        expected = "│ " + footer("builder", 37) + " " + footer("scout", 37) + " │\n"
        self.assertEqual(translate_prose_roles(original), expected)
        self.assertEqual(translate_prose_roles(expected), expected)
        unchanged = footer("asked", 37) + "\n" + footer("explorer-seat", 38)
        self.assertEqual(translate_prose_roles(unchanged), unchanged)
        self.assertEqual(translate_prose_roles("fixer ─────\n"), "builder ─────\n")

    def test_case_insensitive_word_forms_are_preserved(self):
        from role_translate import translate_prose_roles

        self.assertEqual(
            translate_prose_roles(
                "Reviewer effort; a reviewer still running; "
                "escalate to the orchestrator; REVIEWER seats"
            ),
            "Critic effort; a critic still running; "
            "escalate to the nexus; CRITIC seats",
        )

    def test_review_policy_python_prose_matches_rust_after_translation(self):
        from role_translate import translate_prose_roles

        python = (
            "Low-risk batching: on — up to 6 merged low-risk lane(s) "
            "accumulate for one Reviewer over several lanes.\n"
            "After 2 REVISE round(s), stop asking for another review and "
            "escalate to the orchestrator instead.\n"
            "Reviewer effort: medium.\n"
            "Timeout: 30 minute(s) — `multiverse sweep` flags a reviewer "
            "still running past this.\n"
            "Run tests: no — the orchestrator already ran the mutations "
            "and the full suite; read and reason only, do not rerun the suite.\n"
            "Reviewer dispatch budget: 3 per project per rolling 24h window\n"
            "\n"
            "reviewer seats:\n"
            "  (no project configures a reviewer chain)\n"
            "\n"
            "reviewer dispatch budget:\n"
            "  sandbox: 0/3 in the last 24h\n"
        )
        self.assertEqual(
            translate_prose_roles(python),
            "Low-risk batching: on — up to 6 merged low-risk lane(s) "
            "accumulate for one Critic over several lanes.\n"
            "After 2 REVISE round(s), stop asking for another review and "
            "escalate to the nexus instead.\n"
            "Critic effort: medium.\n"
            "Timeout: 30 minute(s) — `multiverse sweep` flags a critic "
            "still running past this.\n"
            "Run tests: no — the nexus already ran the mutations "
            "and the full suite; read and reason only, do not rerun the suite.\n"
            "Critic dispatch budget: 3 per project per rolling 24h window\n"
            "\n"
            "critic seats:\n"
            "  (no project configures a critic chain)\n"
            "\n"
            "critic dispatch budget:\n"
            "  sandbox: 0/3 in the last 24h\n",
        )

    def test_account_hyphen_names_are_not_rewritten(self):
        from role_translate import translate_prose_roles

        text = "account=explorer-seat pool=codex:test"
        self.assertEqual(translate_prose_roles(text), text)

    def test_static_orchestrator_noun_boilerplate_is_not_rewritten(self):
        # Card 20261103-b-71fb3f: these three sentences are generic-noun
        # boilerplate rendered identically by both implementations; a
        # translate here would manufacture a diff where none exists.
        from role_translate import translate_prose_roles

        text = (
            "in your brief: attribute your own session, never the "
            "orchestrator's.\n"
            "references. Not file contents — the orchestrator can read a "
            "file it has a\n"
            "path to, and pasted contents cost it the context this role "
            "exists to save.>\n"
            "silence, because the orchestrator will otherwise assume "
            "coverage.>\n"
        )
        self.assertEqual(translate_prose_roles(text), text)

    def test_dynamic_orchestrator_role_word_near_static_noun_text(self):
        # The exclusion is anchored to the exact static suffixes, not to
        # "orchestrator" generally — a real role-rename use right next to
        # one of the static sentences still converts.
        from role_translate import translate_prose_roles

        text = "escalate to the orchestrator; never the orchestrator's."
        self.assertEqual(
            translate_prose_roles(text),
            "escalate to the nexus; never the orchestrator's.",
        )

    def test_a_genuine_orchestrators_possessive_not_preceded_by_never_the_still_converts(
        self,
    ):
        # Card `20261103-fixer-2faef1` finding 5: the bare `'s\.` suffix
        # used to exempt *any* sentence ending "orchestrator's.", not only
        # this template's own "never the orchestrator's." — a role-name
        # mention with the same ending, but a different lead-in, must
        # still convert.
        from role_translate import translate_prose_roles

        text = "the final call on this card belongs to the orchestrator's."
        self.assertEqual(
            translate_prose_roles(text),
            "the final call on this card belongs to the nexus's.",
        )


if __name__ == "__main__":
    unittest.main()
