import 'package:flutter_test/flutter_test.dart';
import 'package:zcash_wallet/src/features/swap/models/swap_deposit_qr_payload.dart';

void main() {
  test('keeps plain deposit addresses unchanged without memo', () {
    expect(
      swapDepositQrPayload('t1deposit-address', null),
      't1deposit-address',
    );
    expect(
      swapDepositQrPayload('t1deposit-address', '  '),
      't1deposit-address',
    );
  });

  test('encodes deposit memo as a query component', () {
    expect(
      swapDepositQrPayload('t1deposit-address', 'memo with & routing=value?'),
      't1deposit-address?memo=memo+with+%26+routing%3Dvalue%3F',
    );
  });

  test('appends memo with ampersand when payload already has a query', () {
    expect(
      swapDepositQrPayload('t1deposit-address?amount=1', 'memo-7'),
      't1deposit-address?amount=1&memo=memo-7',
    );
  });
}
