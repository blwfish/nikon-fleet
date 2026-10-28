import pytest

from conftest import required, skip_or_fail


class TestRequired:
    @pytest.mark.parametrize("value", ["1", "true", "yes", "anything"])
    def test_a_set_switch_requires(self, value):
        assert required("CI", "REQUIRE_CLI", env={"REQUIRE_CLI": value})
        assert required("CI", "REQUIRE_CLI", env={"CI": value})

    @pytest.mark.parametrize("value", ["", "0", "false"])
    def test_empty_zero_and_false_do_not(self, value):
        assert not required("CI", "REQUIRE_CLI", env={"CI": value, "REQUIRE_CLI": value})

    def test_unset_does_not(self):
        assert not required("CI", "REQUIRE_CLI", env={})

    def test_an_unrelated_switch_does_not(self):
        assert not required("CI", "REQUIRE_CLI", env={"REQUIRE_DISPLAY": "1"})


class TestSkipOrFail:
    def test_skips_by_default(self, monkeypatch):
        monkeypatch.delenv("CI", raising=False)
        monkeypatch.delenv("REQUIRE_DISPLAY", raising=False)
        with pytest.raises(BaseException) as e:
            skip_or_fail("no display", "REQUIRE_DISPLAY")
        assert e.type is pytest.skip.Exception

    @pytest.mark.parametrize("name", ["CI", "REQUIRE_DISPLAY"])
    def test_fails_when_required(self, monkeypatch, name):
        monkeypatch.delenv("CI", raising=False)
        monkeypatch.setenv(name, "1")
        with pytest.raises(BaseException) as e:       # a skip must not pass for a failure
            skip_or_fail("no display", "REQUIRE_DISPLAY")
        assert e.type is pytest.fail.Exception and "no display" in str(e.value)

    def test_zero_still_skips(self, monkeypatch):
        monkeypatch.setenv("CI", "0")
        monkeypatch.setenv("REQUIRE_DISPLAY", "false")
        with pytest.raises(BaseException) as e:
            skip_or_fail("no display", "REQUIRE_DISPLAY")
        assert e.type is pytest.skip.Exception
