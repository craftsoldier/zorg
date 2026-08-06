import 'dart:async';

import 'package:flutter/services.dart';
import 'package:flutter/widgets.dart';

import 'app_toast.dart';

void copyTextWithToast(
  BuildContext context, {
  required String text,
  required String toastMessage,
}) {
  unawaited(
    _copyTextWithToast(context, text: text, toastMessage: toastMessage),
  );
}

Future<void> _copyTextWithToast(
  BuildContext context, {
  required String text,
  required String toastMessage,
}) async {
  try {
    await Clipboard.setData(ClipboardData(text: text));
  } catch (_) {
    return;
  }
  if (!context.mounted) return;
  showAppToast(context, toastMessage);
}
