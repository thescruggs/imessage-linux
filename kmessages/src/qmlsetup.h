// SPDX-License-Identifier: SSPL-1.0
#pragma once

#include <QQuickImageProvider>
#include <QUrl>

class QQmlEngine;

namespace km
{
class AppController;

/// QML import "org.imessage.kmessages": singleton `App` (the controller) and the model types.
void registerQmlTypes(AppController *controller);
/// i18n() in QML, plus registerQmlTypes().
void setupQmlEngine(QQmlEngine &engine, AppController *controller);
QUrl mainQmlUrl();

/// "image://attachment/<percent-encoded local path>": the file decoded with
/// its EXIF orientation applied (QML's Image does that with autoTransform,
/// Kirigami.ShadowedImage cannot), scaled to the requested width.
class AttachmentImageProvider : public QQuickImageProvider
{
public:
    AttachmentImageProvider();
    QImage requestImage(const QString &id, QSize *size, const QSize &requestedSize) override;
};

}
