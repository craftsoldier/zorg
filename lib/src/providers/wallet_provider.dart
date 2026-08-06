import 'dart:async';

import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../main.dart' show log;
import '../app_bootstrap.dart';
import 'account_provider.dart';

class WalletState {
  final bool hasWallet;
  final String? unifiedAddress;
  final String? network;
  final String? activeAccountUuid;

  const WalletState({
    this.hasWallet = false,
    this.unifiedAddress,
    this.network,
    this.activeAccountUuid,
  });
}

class WalletNotifier extends AsyncNotifier<WalletState> {
  @override
  FutureOr<WalletState> build() {
    final bootstrap = ref.watch(appBootstrapProvider);
    final accountState = ref.watch(accountProvider);

    return accountState.when(
      data: (state) {
        if (!state.hasAccounts) return const WalletState();
        return WalletState(
          hasWallet: true,
          unifiedAddress: state.activeAddress,
          activeAccountUuid: state.activeAccountUuid,
        );
      },
      loading: () => WalletState(
        hasWallet: bootstrap.hasWallet,
        unifiedAddress: bootstrap.initialAccountState.activeAddress,
        activeAccountUuid: bootstrap.initialAccountState.activeAccountUuid,
      ),
      error: (e, st) {
        log('WalletNotifier: error from accountProvider: $e');
        Error.throwWithStackTrace(e, st);
      },
    );
  }
}

final walletProvider = AsyncNotifierProvider<WalletNotifier, WalletState>(
  WalletNotifier.new,
);
