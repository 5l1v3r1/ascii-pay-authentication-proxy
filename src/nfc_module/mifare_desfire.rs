use std::sync::mpsc::Sender;

use crate::http_client::*;
use crate::nfc::{mifare_desfire, utils, MiFareDESFire, NfcResult};
use crate::Message;

const DEFAULT_KEY: [u8; 16] = hex!("00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00");
const PICC_KEY: [u8; 16] = hex!("00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00");
const PICC_APPLICATION: [u8; 3] = hex!("00 00 00");
const ASCII_APPLICATION: [u8; 3] = hex!("C0 FF EE");
const ASCII_SECRET_FILE_NUMBER: u8 = 0;

fn is_writeable(card: &MiFareDESFire) -> NfcResult<bool> {
    card.select_application(PICC_APPLICATION)?;
    card.authenticate(0, &PICC_KEY)?;
    Ok(true)
}

fn create_response(_secret: &str, challenge: &str) -> NfcResult<String> {
    // TODO
    Ok(challenge.to_owned())
}

fn init_ascii_card(card: &MiFareDESFire, key: &str, secret: &str) -> NfcResult<()> {
    let key = utils::str_to_bytes(key);
    let secret = utils::str_to_bytes(secret);

    card.select_application(PICC_APPLICATION)?;
    card.authenticate(0, &PICC_KEY)?;

    let application_ids = card.get_application_ids()?;
    if application_ids.contains(&ASCII_APPLICATION) {
        card.delete_application(ASCII_APPLICATION)?;
    }

    card.create_application(
        ASCII_APPLICATION,
        mifare_desfire::KeySettings {
            access_rights: mifare_desfire::KeySettingsAccessRights::MasterKey,
            master_key_settings_changeable: true,
            master_key_not_required_create_delete: false,
            master_key_not_required_directory_access: false,
            master_key_changeable: true,
        },
        1,
    )?;
    card.select_application(ASCII_APPLICATION)?;
    let session_key = card.authenticate(0, &DEFAULT_KEY)?;

    card.change_key(0, true, &DEFAULT_KEY, &key, &session_key)?;
    let session_key = card.authenticate(0, &key)?;
    card.change_key_settings(
        &mifare_desfire::KeySettings {
            access_rights: mifare_desfire::KeySettingsAccessRights::MasterKey,
            master_key_settings_changeable: false,
            master_key_not_required_create_delete: false,
            master_key_not_required_directory_access: false,
            master_key_changeable: false,
        },
        &session_key,
    )?;

    card.create_std_data_file(
        ASCII_SECRET_FILE_NUMBER,
        mifare_desfire::FileSettingsCommunication::Enciphered,
        mifare_desfire::FileSettingsAccessRights {
            read: mifare_desfire::FileSettingsAccessRightsKey::MasterKey,
            write: mifare_desfire::FileSettingsAccessRightsKey::MasterKey,
            read_write: mifare_desfire::FileSettingsAccessRightsKey::MasterKey,
            change_access: mifare_desfire::FileSettingsAccessRightsKey::MasterKey,
        },
        secret.len() as u32,
    )?;

    card.write_data(
        ASCII_SECRET_FILE_NUMBER,
        0,
        &secret,
        mifare_desfire::Encryption::Encrypted(session_key.clone()),
    )?;

    Ok(())
}

pub fn handle(sender: &Sender<Message>, card: &MiFareDESFire) -> NfcResult<()> {
    let card_id = format!(
        "{}:{}",
        utils::bytes_to_string(&card.card.get_atr()?),
        utils::bytes_to_string(&card.get_version()?.id()),
    );

    let response = if let Some(response) = send_identify(IdentificationRequest::Nfc {
        id: card_id.clone(),
    }) {
        response
    } else {
        return Ok(());
    };

    let (_, key, challenge) = match response {
        IdentificationResponse::Account { account } => {
            if sender.send(Message::Account { account }).is_err() {
                // TODO Error
            }
            return Ok(());
        }
        IdentificationResponse::Product { product } => {
            if sender.send(Message::Product { product }).is_err() {
                // TODO Error
            }
            return Ok(());
        }
        IdentificationResponse::NotFound => {
            let writeable = is_writeable(&card).unwrap_or(false);
            if sender
                .send(Message::NfcCard {
                    id: card_id,
                    writeable,
                })
                .is_err()
            {
                // TODO Error
            }
            return Ok(());
        }
        IdentificationResponse::AuthenticationNeeded { id, key, challenge } => {
            if card_id != id {
                return Ok(());
            }
            (id, key, challenge)
        }
        IdentificationResponse::WriteKey { id, key, secret } => {
            if card_id != id {
                return Ok(());
            }

            // Write auth key and secret to card
            init_ascii_card(&card, &key, &secret)?;

            // Request challenge token
            let response = if let Some(response) = send_identify(IdentificationRequest::Nfc {
                id: card_id.clone(),
            }) {
                response
            } else {
                return Ok(());
            };

            // Reponse should always be `AuthenticationNeeded`
            if let IdentificationResponse::AuthenticationNeeded { id, key, challenge } = response {
                if card_id != id {
                    return Ok(());
                }
                (id, key, challenge)
            } else {
                return Ok(());
            }
        }
    };

    let key = utils::str_to_bytes(&key);

    card.select_application(ASCII_APPLICATION)?;
    let session_key = card.authenticate(0, &key)?;

    let secret = card.read_data(0, 0, 0, mifare_desfire::Encryption::Encrypted(session_key))?;
    let secret = utils::bytes_to_string(&secret);
    let response = create_response(&secret, &challenge)?;

    let response = if let Some(response) = send_identify(IdentificationRequest::NfcSecret {
        id: card_id,
        challenge,
        response,
    }) {
        response
    } else {
        return Ok(());
    };

    match response {
        IdentificationResponse::Account { account } => {
            if sender.send(Message::Account { account }).is_err() {
                // TODO Error
            }
        }
        IdentificationResponse::Product { product } => {
            if sender.send(Message::Product { product }).is_err() {
                // TODO Error
            }
        }
        _ => {}
    };

    Ok(())
}

pub fn handle_payment(
    sender: &Sender<Message>,
    card: &MiFareDESFire,
    amount: i32,
) -> NfcResult<()> {
    let card_id = format!(
        "{}:{}",
        utils::bytes_to_string(&card.card.get_atr()?),
        utils::bytes_to_string(&card.get_version()?.id()),
    );

    let response = if let Some(response) = send_token_request(TokenRequest {
        amount,
        method: Authentication::Nfc {
            id: card_id.clone(),
        },
    }) {
        response
    } else {
        return Ok(());
    };

    let (_, key, challenge) = match response {
        TokenResponse::Authorized { token } => {
            if sender.send(Message::PaymentToken { token }).is_err() {
                // TODO Error
            }
            return Ok(());
        }
        TokenResponse::AuthenticationNeeded { id, key, challenge } => {
            if card_id != id {
                return Ok(());
            }
            (id, key, challenge)
        }
    };

    let key = utils::str_to_bytes(&key);

    card.select_application(ASCII_APPLICATION)?;
    let session_key = card.authenticate(0, &key)?;

    let secret = card.read_data(0, 0, 0, mifare_desfire::Encryption::Encrypted(session_key))?;
    let secret = utils::bytes_to_string(&secret);
    let response = create_response(&secret, &challenge)?;

    let response = if let Some(response) = send_token_request(TokenRequest {
        amount,
        method: Authentication::NfcSecret {
            id: card_id,
            challenge,
            response,
        },
    }) {
        response
    } else {
        return Ok(());
    };

    if let TokenResponse::Authorized { token } = response {
        if sender.send(Message::PaymentToken { token }).is_err() {
            // TODO Error
        }
    };

    Ok(())
}
