import 'package:flutter/material.dart';

const accents = <String, Color>{
  'orange': Color(0xfff0692a),
  'blue': Color(0xff2f6fed),
  'green': Color(0xff10a37f),
  'purple': Color(0xff8b5cf6),
  'pink': Color(0xffe0457b),
};

Color accentColor(String name) => accents[name] ?? accents['orange']!;

@immutable
class HubColors extends ThemeExtension<HubColors> {
  final Color background, sidebar, border, hover, selected, text, muted, subtle;
  final Color bubble, composer, menu, code, accent, onAccent, danger, success;
  final Color warning;
  const HubColors({
    required this.background,
    required this.sidebar,
    required this.border,
    required this.hover,
    required this.selected,
    required this.text,
    required this.muted,
    required this.subtle,
    required this.bubble,
    required this.composer,
    required this.menu,
    required this.code,
    required this.accent,
    required this.onAccent,
    required this.danger,
    required this.success,
    required this.warning,
  });

  factory HubColors.light(Color accent) => HubColors(
    background: const Color(0xffffffff),
    sidebar: const Color(0xfff9f9f9),
    border: const Color(0xffe5e5e5),
    hover: const Color(0xffefefef),
    selected: const Color(0xffe8e8e8),
    text: const Color(0xff0d0d0d),
    muted: const Color(0xff5d5d5d),
    subtle: const Color(0xff8f8f8f),
    bubble: const Color(0xfff4f4f4),
    composer: const Color(0xffffffff),
    menu: const Color(0xffffffff),
    code: const Color(0xfff7f7f7),
    accent: accent,
    onAccent: const Color(0xffffffff),
    danger: const Color(0xffd93025),
    success: const Color(0xff10a37f),
    warning: const Color(0xffe8a317),
  );

  factory HubColors.dark(Color accent) => HubColors(
    background: const Color(0xff212121),
    sidebar: const Color(0xff181818),
    border: const Color(0x1affffff),
    hover: const Color(0xff2a2a2a),
    selected: const Color(0xff303030),
    text: const Color(0xffececec),
    muted: const Color(0xffb4b4b4),
    subtle: const Color(0xff8f8f8f),
    bubble: const Color(0xff303030),
    composer: const Color(0xff303030),
    menu: const Color(0xff353535),
    code: const Color(0xff171717),
    accent: accent,
    onAccent: const Color(0xffffffff),
    danger: const Color(0xffff6b5e),
    success: const Color(0xff3ecf9e),
    warning: const Color(0xfff2c14e),
  );

  /// Falls back for widgets pumped under a bare MaterialApp (widget tests).
  static HubColors of(BuildContext context) {
    final theme = Theme.of(context);
    return theme.extension<HubColors>() ??
        (theme.brightness == Brightness.dark
            ? HubColors.dark(accentColor('orange'))
            : HubColors.light(accentColor('orange')));
  }

  @override
  HubColors copyWith() => this;

  @override
  HubColors lerp(covariant HubColors? other, double t) {
    if (other == null) return this;
    Color l(Color a, Color b) => Color.lerp(a, b, t)!;
    return HubColors(
      background: l(background, other.background),
      sidebar: l(sidebar, other.sidebar),
      border: l(border, other.border),
      hover: l(hover, other.hover),
      selected: l(selected, other.selected),
      text: l(text, other.text),
      muted: l(muted, other.muted),
      subtle: l(subtle, other.subtle),
      bubble: l(bubble, other.bubble),
      composer: l(composer, other.composer),
      menu: l(menu, other.menu),
      code: l(code, other.code),
      accent: l(accent, other.accent),
      onAccent: l(onAccent, other.onAccent),
      danger: l(danger, other.danger),
      success: l(success, other.success),
      warning: l(warning, other.warning),
    );
  }
}

const fontFamily = 'RyzHub Sans';
const fontFallback = ['Microsoft YaHei', 'Noto Sans CJK SC', 'PingFang SC'];

/// Masked fields: the UI font's bullet is a full-width CJK dot, so masks set
/// in 0xProto's Latin one.
const obscuredStyle = TextStyle(fontFamily: '0xProto');

/// Model identifiers and reasoning levels: even glyph widths keep the label
/// steady as values change. CJK labels fall back to the UI font.
TextStyle modelLabelStyle({required double size, required Color color}) =>
    TextStyle(
      fontFamily: '0xProto',
      fontFamilyFallback: const [fontFamily, ...fontFallback],
      fontSize: size,
      color: color,
      height: 1.2,
      leadingDistribution: TextLeadingDistribution.even,
    );

/// Theme-level styles replace, rather than merge with, the text theme, so each
/// one names the bundled CJK font explicitly.
TextStyle _font({double? fontSize, Color? color}) => TextStyle(
  fontFamily: fontFamily,
  fontFamilyFallback: fontFallback,
  fontSize: fontSize,
  color: color,
  leadingDistribution: TextLeadingDistribution.even,
);

/// Extra line height goes equally above and below the glyphs. The default
/// splits it by the font's ascent:descent ratio, which pushes text down.
TextTheme _evenLeading(TextTheme t) {
  TextStyle? even(TextStyle? s) =>
      s?.copyWith(leadingDistribution: TextLeadingDistribution.even);
  return TextTheme(
    displayLarge: even(t.displayLarge),
    displayMedium: even(t.displayMedium),
    displaySmall: even(t.displaySmall),
    headlineLarge: even(t.headlineLarge),
    headlineMedium: even(t.headlineMedium),
    headlineSmall: even(t.headlineSmall),
    titleLarge: even(t.titleLarge),
    titleMedium: even(t.titleMedium),
    titleSmall: even(t.titleSmall),
    bodyLarge: even(t.bodyLarge),
    bodyMedium: even(t.bodyMedium),
    bodySmall: even(t.bodySmall),
    labelLarge: even(t.labelLarge),
    labelMedium: even(t.labelMedium),
    labelSmall: even(t.labelSmall),
  );
}

ThemeData buildTheme(Brightness brightness, Color accent) {
  final c = brightness == Brightness.dark
      ? HubColors.dark(accent)
      : HubColors.light(accent);
  final scheme = ColorScheme.fromSeed(seedColor: accent, brightness: brightness)
      .copyWith(
        primary: accent,
        onPrimary: c.onAccent,
        secondary: accent,
        surface: c.background,
        onSurface: c.text,
        onSurfaceVariant: c.muted,
        outline: c.border,
        outlineVariant: c.border,
        surfaceContainerLowest: c.background,
        surfaceContainerLow: c.sidebar,
        surfaceContainer: c.menu,
        surfaceContainerHigh: c.menu,
        surfaceContainerHighest: c.bubble,
        error: c.danger,
      );
  final radius = BorderRadius.circular(12);
  final theme = ThemeData(
    useMaterial3: true,
    brightness: brightness,
    colorScheme: scheme,
    fontFamily: fontFamily,
    fontFamilyFallback: fontFallback,
    scaffoldBackgroundColor: c.background,
    canvasColor: c.background,
    extensions: [c],
    splashFactory: InkSparkle.constantTurbulenceSeedSplashFactory,
    hoverColor: c.hover,
    dividerTheme: DividerThemeData(color: c.border, thickness: 1, space: 1),
    iconTheme: IconThemeData(color: c.muted, size: 20),
    textSelectionTheme: TextSelectionThemeData(
      cursorColor: accent,
      selectionColor: accent.withValues(alpha: .25),
    ),
    inputDecorationTheme: InputDecorationTheme(
      isDense: true,
      contentPadding: const EdgeInsets.symmetric(horizontal: 14, vertical: 14),
      border: OutlineInputBorder(
        borderRadius: radius,
        borderSide: BorderSide(color: c.border),
      ),
      enabledBorder: OutlineInputBorder(
        borderRadius: radius,
        borderSide: BorderSide(color: c.border),
      ),
      focusedBorder: OutlineInputBorder(
        borderRadius: radius,
        borderSide: BorderSide(color: c.muted),
      ),
      labelStyle: _font(color: c.muted),
      hintStyle: _font(color: c.subtle),
    ),
    popupMenuTheme: PopupMenuThemeData(
      color: c.menu,
      surfaceTintColor: Colors.transparent,
      elevation: 6,
      shadowColor: Colors.black.withValues(alpha: .18),
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(16),
        side: BorderSide(color: c.border),
      ),
      textStyle: _font(fontSize: 14, color: c.text),
      menuPadding: const EdgeInsets.all(8),
    ),
    menuTheme: MenuThemeData(
      style: MenuStyle(
        backgroundColor: WidgetStatePropertyAll(c.menu),
        surfaceTintColor: const WidgetStatePropertyAll(Colors.transparent),
        shape: WidgetStatePropertyAll(
          RoundedRectangleBorder(
            borderRadius: BorderRadius.circular(16),
            side: BorderSide(color: c.border),
          ),
        ),
        elevation: const WidgetStatePropertyAll(6),
        shadowColor: WidgetStatePropertyAll(
          Colors.black.withValues(alpha: .18),
        ),
        padding: const WidgetStatePropertyAll(EdgeInsets.all(8)),
      ),
    ),
    // Items sit inset from the panel edge with a rounded highlight.
    menuButtonTheme: MenuButtonThemeData(
      style: ButtonStyle(
        padding: const WidgetStatePropertyAll(
          EdgeInsets.symmetric(horizontal: 14),
        ),
        minimumSize: const WidgetStatePropertyAll(Size(0, 38)),
        shape: WidgetStatePropertyAll(
          RoundedRectangleBorder(borderRadius: BorderRadius.circular(12)),
        ),
        foregroundColor: WidgetStatePropertyAll(c.text),
        iconColor: WidgetStatePropertyAll(c.muted),
        overlayColor: WidgetStatePropertyAll(c.hover),
        textStyle: WidgetStatePropertyAll(_font(fontSize: 14)),
      ),
    ),
    tooltipTheme: TooltipThemeData(
      waitDuration: const Duration(milliseconds: 500),
      decoration: BoxDecoration(
        color: brightness == Brightness.dark ? c.menu : c.text,
        borderRadius: BorderRadius.circular(8),
      ),
      textStyle: _font(
        fontSize: 12,
        color: brightness == Brightness.dark ? c.text : c.background,
      ),
    ),
    dialogTheme: DialogThemeData(
      backgroundColor: c.menu,
      surfaceTintColor: Colors.transparent,
      shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(20)),
    ),
    drawerTheme: DrawerThemeData(
      backgroundColor: c.sidebar,
      surfaceTintColor: Colors.transparent,
      shape: const RoundedRectangleBorder(),
      width: 280,
    ),
    snackBarTheme: SnackBarThemeData(
      behavior: SnackBarBehavior.floating,
      backgroundColor: brightness == Brightness.dark ? c.menu : c.text,
      contentTextStyle: _font(
        color: brightness == Brightness.dark ? c.text : c.background,
      ),
      shape: RoundedRectangleBorder(borderRadius: radius),
      width: 420,
    ),
    switchTheme: SwitchThemeData(
      thumbColor: const WidgetStatePropertyAll(Colors.white),
      trackColor: WidgetStateProperty.resolveWith(
        (s) => s.contains(WidgetState.selected) ? accent : c.border,
      ),
      trackOutlineColor: const WidgetStatePropertyAll(Colors.transparent),
      thumbIcon: const WidgetStatePropertyAll(null),
    ),
    filledButtonTheme: FilledButtonThemeData(
      style: FilledButton.styleFrom(
        backgroundColor: c.text,
        foregroundColor: c.background,
        shape: const StadiumBorder(),
        padding: const EdgeInsets.symmetric(horizontal: 18, vertical: 12),
      ),
    ),
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: OutlinedButton.styleFrom(
        foregroundColor: c.text,
        side: BorderSide(color: c.border),
        shape: const StadiumBorder(),
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 10),
      ),
    ),
    textButtonTheme: TextButtonThemeData(
      style: TextButton.styleFrom(
        foregroundColor: c.text,
        shape: const StadiumBorder(),
      ),
    ),
    iconButtonTheme: IconButtonThemeData(
      style: IconButton.styleFrom(
        foregroundColor: c.muted,
        // Standard density keeps room between a glyph and its highlight; the
        // compact desktop default hugs it.
        visualDensity: VisualDensity.standard,
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(10)),
      ),
    ),
    chipTheme: ChipThemeData(
      backgroundColor: c.composer,
      selectedColor: accent.withValues(alpha: .14),
      checkmarkColor: accent,
      side: BorderSide(color: c.border),
      shape: const StadiumBorder(),
      labelStyle: _font(fontSize: 13, color: c.text),
      padding: const EdgeInsets.symmetric(horizontal: 4),
    ),
    segmentedButtonTheme: SegmentedButtonThemeData(
      style: SegmentedButton.styleFrom(
        backgroundColor: c.bubble,
        foregroundColor: c.subtle,
        selectedBackgroundColor: c.background,
        selectedForegroundColor: c.text,
        side: BorderSide.none,
        shape: const StadiumBorder(),
        visualDensity: VisualDensity.compact,
        textStyle: _font(fontSize: 13),
      ),
    ),
    progressIndicatorTheme: ProgressIndicatorThemeData(color: accent),
  );
  return theme.copyWith(
    textTheme: _evenLeading(theme.textTheme),
    primaryTextTheme: _evenLeading(theme.primaryTextTheme),
  );
}
