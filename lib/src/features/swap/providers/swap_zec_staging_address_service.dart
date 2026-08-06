import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../../../../main.dart' show log;
import '../../../providers/receive_address_provider.dart';
import '../domain/swap_address_plan.dart';
import '../domain/swap_contract.dart';

final swapZecStagingAddressServiceProvider =
    Provider<SwapZecStagingAddressService>((ref) {
      return SwapZecStagingAddressService(
        loadCurrentShieldedAddress: ({required accountUuid}) {
          return ref
              .read(receiveAddressServiceProvider)
              .loadShieldedAddress(accountUuid: accountUuid);
        },
        prepareFreshShieldedAddress: ({required accountUuid}) {
          return _loadSwapShieldedRecipientAddress(
            ref,
            accountUuid: accountUuid,
          );
        },
      );
    });

typedef LoadShieldedAddress =
    Future<String> Function({required String accountUuid});

Future<String> _loadSwapShieldedRecipientAddress(
  Ref ref, {
  required String accountUuid,
}) async {
  final receiveAddressService = ref.read(receiveAddressServiceProvider);
  try {
    return await receiveAddressService.renewShieldedAddress(
      accountUuid: accountUuid,
    );
  } catch (e) {
    if (!_isSaplingReceiverUnsupported(e)) rethrow;
    log(
      'SwapZecStagingAddressService: diversified Sapling+Orchard UA '
      'generation is unavailable; using current shielded UA: $e',
    );
    return receiveAddressService.loadShieldedAddress(accountUuid: accountUuid);
  }
}

bool _isSaplingReceiverUnsupported(Object error) {
  return error.toString().contains(
    'Unified Address generation does not yet support receivers of type Sapling',
  );
}

class SwapZecStagingAddress {
  const SwapZecStagingAddress({required this.address});

  final String address;

  SwapAddressPlan toAddressPlan({
    required SwapDirection direction,
    required SwapAsset externalAsset,
    required String userExternalAddress,
  }) {
    return SwapAddressPlan.fromUserInput(
      direction: direction,
      externalAsset: externalAsset,
      userExternalAddress: userExternalAddress,
      walletZecAddress: address,
    );
  }
}

class SwapZecStagingAddressUnavailableException implements Exception {
  const SwapZecStagingAddressUnavailableException(this.cause);

  final Object cause;

  @override
  String toString() {
    return 'Could not prepare a fresh wallet receive address. '
        'Retry after wallet sync or close older pending swaps before requesting a new quote.';
  }
}

class SwapZecStagingAddressService {
  const SwapZecStagingAddressService({
    required LoadShieldedAddress loadCurrentShieldedAddress,
    LoadShieldedAddress? prepareFreshShieldedAddress,
  }) : _prepareFreshShieldedAddress =
           prepareFreshShieldedAddress ?? loadCurrentShieldedAddress;

  final LoadShieldedAddress _prepareFreshShieldedAddress;

  Future<SwapZecStagingAddress> prepareForQuote({
    required String accountUuid,
  }) async {
    return _prepareShieldedRecipient(
      accountUuid,
      loadShieldedAddress: _prepareFreshShieldedAddress,
      operationLabel: 'quote',
    );
  }

  Future<SwapZecStagingAddress> _prepareShieldedRecipient(
    String accountUuid, {
    required LoadShieldedAddress loadShieldedAddress,
    required String operationLabel,
  }) async {
    try {
      final address = await loadShieldedAddress(accountUuid: accountUuid);
      return SwapZecStagingAddress(address: address);
    } catch (e) {
      log(
        'SwapZecStagingAddressService: shielded receive address preparation '
        'failed; blocking $operationLabel: $e',
      );
      throw SwapZecStagingAddressUnavailableException(e);
    }
  }
}
