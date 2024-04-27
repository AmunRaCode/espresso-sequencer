# Purpose

This subfolder was created to test various upgrade scenarios when using the UUPS (Universal Upgradeable Proxy Standard) proxy pattern.

## How to Run the Tests

Execute the following command to run the tests targeting the `Box` contract:

```bash
forge test --match-contract Box -vvv --summary


## Tests

The tests verify the following aspects after an upgrade:

Struct Modification: Checks for the addition of new members to a struct.
Enum Modification: Ensures new members can be added to enums.
ETH Deposit: Confirms that ETH deposits are retained in the upgraded contract version.
Withdrawal Function: Tests the functionality of a withdrawal method introduced after the initial deployment.
These tests ensure that the contract upgrades correctly handle enhancements and maintain state consistency.
